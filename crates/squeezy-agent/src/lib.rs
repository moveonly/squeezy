use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextAttachmentSource, ContextAttachmentStatus, CostSnapshot,
    DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES, PROJECT_SETTINGS_FILE, PermissionAction,
    PermissionCapability, PermissionRequest, PermissionRule, PermissionRuleSource, PermissionScope,
    PermissionVerdict, Redactor, SessionMetrics, SessionMode, SqueezyError, StreamRedactor,
    TranscriptItem, TurnId, TurnMetrics, context_attachment_preview,
    context_attachment_storage_text, default_settings_path, detect_context_attachment_kind,
    escape_toml_basic_string,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmToolSpec, estimate_cost};
use squeezy_store::{
    CleanupReport, ResumeItem, SessionEvent, SessionHandle, SessionMetadata, SessionQuery,
    SessionRecord, SessionResumeState, SessionStatus, SessionStore, SqueezyStore,
    StoredToolReceipt,
};
use squeezy_telemetry::{
    ErrorKind, TelemetryClient, TelemetryEvent, ToolCostProperties,
    ToolStatusKind as TelemetryToolStatusKind, ToolTelemetryReport,
};
use squeezy_tools::{
    ToolCall, ToolCostHint, ToolOutputConfig, ToolReceipt, ToolRegistry, ToolRegistryRuntime,
    ToolResult, ToolSpec, ToolStatus, WebToolConfig, sha256_hex,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_TOOL_ROUNDS: usize = 8;

#[derive(Debug, Clone, Default)]
struct ConversationState {
    previous_response_id: Option<String>,
    conversation: Vec<LlmInputItem>,
    transcript: Vec<TranscriptItem>,
    context_attachments: Vec<ContextAttachment>,
    cost: CostSnapshot,
    metrics: SessionMetrics,
    redactions: u64,
}

impl ConversationState {
    fn from_resume(state: SessionResumeState, metadata: &SessionMetadata) -> Self {
        Self {
            previous_response_id: state.previous_response_id,
            conversation: state
                .conversation
                .into_iter()
                .map(resume_item_to_llm_input)
                .collect(),
            transcript: state.transcript,
            context_attachments: state.context_attachments,
            cost: metadata.cost.clone(),
            metrics: metadata.metrics.clone(),
            redactions: metadata.redactions,
        }
    }

    fn to_resume_state(&self) -> SessionResumeState {
        SessionResumeState {
            resume_available: true,
            previous_response_id: self.previous_response_id.clone(),
            conversation: self
                .conversation
                .iter()
                .cloned()
                .map(llm_input_to_resume_item)
                .collect(),
            transcript: self.transcript.clone(),
            context_attachments: self.context_attachments.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAttachmentUpdate {
    pub attachment: ContextAttachment,
    pub duplicate: bool,
    pub active: bool,
}

#[derive(Clone)]
pub struct Agent {
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    telemetry: TelemetryClient,
    redactor: Arc<Redactor>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    next_turn_id: Arc<AtomicU64>,
    next_approval_id: Arc<AtomicU64>,
    next_attachment_id: Arc<AtomicU64>,
    /// In-memory permission rules added via "Allow user/project rule" during
    /// the current process. Persisted to disk on a best-effort basis; this
    /// vector also makes the rule take effect immediately for subsequent
    /// tool calls without having to wait for a settings reload.
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    /// Active session mode. Stored as an `AtomicU8` so reads on the hot
    /// permission/advertisement paths cannot deadlock, cannot be poisoned by
    /// a panicking writer, and never need a fallback enum value: every byte
    /// we observe was previously written via `SessionMode::to_u8`.
    session_mode: Arc<AtomicU8>,
    store: Option<Arc<SqueezyStore>>,
}

impl Agent {
    pub fn new(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        let session_log = start_session_log(&config, provider.name());
        Self::build(config, provider, session_log, ConversationState::default())
    }

    pub fn resume(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_id: &str,
    ) -> squeezy_core::Result<(Self, Vec<TranscriptItem>)> {
        let store = SessionStore::open(&config);
        let handle = store.open_session(session_id.to_string());
        let resume_state = handle.read_resume_state()?;
        if !resume_state.resume_available {
            return Err(SqueezyError::Agent(format!(
                "session {session_id} is not resumable"
            )));
        }
        let metadata = handle.metadata()?;
        let transcript = resume_state.transcript.clone();
        let conversation_state = ConversationState::from_resume(resume_state, &metadata);
        let agent = Self::build(config, provider, Some(handle.clone()), conversation_state);
        let _ = handle.update_metadata(|metadata| {
            metadata.status = SessionStatus::Running;
            metadata.ended_at_ms = None;
            metadata.resume_available = true;
        });
        let _ = handle.append_event(SessionEvent::new(
            "session_resumed",
            None,
            Some("session resumed".to_string()),
            json!({}),
        ));
        Ok((agent, transcript))
    }

    fn build(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_log: Option<SessionHandle>,
        conversation_state: ConversationState,
    ) -> Self {
        let output_config = ToolOutputConfig {
            spill_threshold_bytes: config.tool_spill_threshold_bytes,
            preview_bytes: config.tool_preview_bytes,
            retention_days: config.tool_output_retention_days,
            output_dir: config.cache.tool_outputs.clone(),
        };
        let web_config = WebToolConfig {
            exa_mcp_url: config.exa_mcp_url.clone(),
            exa_api_key: env::var(&config.exa_api_key_env).ok(),
        };
        // Compile the redactor exactly once and share it with the tool
        // registry. Pattern compilation can never fail here because the
        // surrounding config was already validated when loading.
        let redactor = Arc::new(
            config
                .redaction
                .redactor()
                .expect("validated redaction config must compile"),
        );
        // Open the persistent state store exactly once and share the handle
        // with the tool registry. redb only allows a single live `Database`
        // per file (see `state_store_open_rejects_a_second_handle_on_the_same_file`),
        // so the registry's graph manager must reuse this handle instead of
        // opening its own — otherwise the second open would fail silently
        // and graph partitions would never be persisted.
        let store = SqueezyStore::open(&config.workspace_root, config.cache.root.as_deref())
            .ok()
            .map(Arc::new);
        let runtime = ToolRegistryRuntime::new(store.clone(), redactor.clone());
        let tools = ToolRegistry::new_with_configs_and_skills(
            config.workspace_root.clone(),
            output_config.clone(),
            web_config.clone(),
            config.skills.clone(),
            &config.graph,
            config.permissions.shell_sandbox.clone(),
            runtime.clone(),
        )
        .unwrap_or_else(|_| {
            // Workspace root unavailable; fall back to the current
            // directory but keep the configured redactor and graph
            // policy so the agent never silently downgrades to
            // default patterns or default crawl options.
            ToolRegistry::new_with_configs_and_skills(
                ".",
                output_config,
                web_config,
                config.skills.clone(),
                &config.graph,
                config.permissions.shell_sandbox.clone(),
                runtime,
            )
            .expect("current directory must be a valid tool root")
        });
        let initial_session_mode = config.session_mode;
        let session_metrics = Arc::new(Mutex::new(conversation_state.metrics.clone()));
        let next_attachment_id = next_attachment_counter(&conversation_state.context_attachments);
        Self {
            telemetry: TelemetryClient::from_config(&config),
            config,
            provider,
            tools,
            redactor,
            session_metrics,
            session_log,
            conversation_state: Arc::new(Mutex::new(conversation_state)),
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_approval_id: Arc::new(AtomicU64::new(1)),
            next_attachment_id: Arc::new(AtomicU64::new(next_attachment_id)),
            session_rules: Arc::new(RwLock::new(Vec::new())),
            session_mode: Arc::new(AtomicU8::new(initial_session_mode.to_u8())),
            store,
        }
    }

    /// Snapshot of session-scoped permission rules. Primarily intended for
    /// tests and debug surfaces; the live rule list lives behind a lock and
    /// is consulted on every permission decision.
    pub fn session_rules_snapshot(&self) -> Vec<PermissionRule> {
        self.session_rules
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn session_mode(&self) -> SessionMode {
        load_session_mode(&self.session_mode)
    }

    /// Set the current session mode. Returns true when the mode actually
    /// changed so callers (notably the TUI) can avoid emitting "switched to"
    /// status when the request was a no-op.
    pub fn set_session_mode(&self, mode: SessionMode, source: &'static str) -> bool {
        let previous_u8 = self.session_mode.swap(mode.to_u8(), Ordering::AcqRel);
        let previous = SessionMode::from_u8(previous_u8).unwrap_or_else(|| {
            // Unreachable in practice: every write goes through this method
            // or the constructor, both of which use `to_u8`. Log defensively
            // and treat it as a real change so the new value still wins.
            tracing::warn!(
                target: "squeezy::permissions",
                discriminant = previous_u8,
                "unexpected session mode discriminant; treating as different",
            );
            match mode {
                SessionMode::Plan => SessionMode::Build,
                SessionMode::Build => SessionMode::Plan,
            }
        });
        if previous == mode {
            return false;
        }
        log_session_mode_transition(previous, mode, source);
        true
    }

    pub fn toggle_session_mode(&self, source: &'static str) -> SessionMode {
        let next = match self.session_mode() {
            SessionMode::Plan => SessionMode::Build,
            SessionMode::Build => SessionMode::Plan,
        };
        self.set_session_mode(next, source);
        next
    }

    /// Execute a single tool call from the TUI / local UX path rather than
    /// from inside an agent turn. The "manual" group id mirrors how the agent
    /// labels human-driven invocations so checkpoint grouping stays
    /// consistent across both entry points.
    pub async fn execute_local_tool(&self, call: ToolCall) -> ToolResult {
        self.tools
            .execute_for_group(call, CancellationToken::new(), "manual".to_string())
            .await
    }

    pub async fn flush_telemetry(&self) {
        let _ = self.telemetry.flush().await;
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| handle.session_id().to_string())
    }

    pub fn list_sessions(
        &self,
        query: &SessionQuery,
    ) -> squeezy_core::Result<Vec<SessionMetadata>> {
        SessionStore::open(&self.config).list(query)
    }

    pub fn show_session(&self, session_id: &str) -> squeezy_core::Result<SessionRecord> {
        SessionStore::open(&self.config).show(session_id)
    }

    pub fn export_session(&self, session_id: &str) -> squeezy_core::Result<Value> {
        SessionStore::open(&self.config).export(session_id)
    }

    pub fn cleanup_sessions(&self, ids: &[String]) -> squeezy_core::Result<CleanupReport> {
        // Refuse to delete the session that this agent is currently writing
        // to. Removing it under our feet would orphan future event writes and
        // leave a session that no longer exists on disk but still appears in
        // `metadata`/`resume_state` until the process exits.
        let active = self.session_id();
        if let Some(active_id) = &active
            && ids.iter().any(|id| id == active_id)
        {
            return Err(SqueezyError::Agent(format!(
                "refusing to clean up the active session {active_id}; finish or exit first"
            )));
        }
        SessionStore::open(&self.config).cleanup_excluding(ids, active.as_deref())
    }

    pub fn resume_current(
        &mut self,
        session_id: &str,
    ) -> squeezy_core::Result<Vec<TranscriptItem>> {
        let (agent, transcript) =
            Self::resume(self.config.clone(), self.provider.clone(), session_id)?;
        *self = agent;
        Ok(transcript)
    }

    pub async fn finish_session(&self, status: SessionStatus) {
        let Some(session) = &self.session_log else {
            return;
        };
        let state = self.conversation_state.lock().await.clone();
        let _ = session.write_resume_state(&state.to_resume_state());
        let _ = session.finish(status, state.cost, state.metrics, state.redactions);
    }

    pub async fn attach_pasted_context(
        &self,
        text: String,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        self.attach_context_bytes(
            ContextAttachmentSource::Paste,
            "pasted context".to_string(),
            None,
            text.into_bytes(),
        )
        .await
    }

    pub async fn attach_file_context(
        &self,
        path: PathBuf,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        let resolved = if path.is_absolute() {
            path
        } else {
            self.config.workspace_root.join(path)
        };
        let bytes = fs::read(&resolved)?;
        let label = resolved
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attached file")
            .to_string();
        let display_path = resolved
            .strip_prefix(&self.config.workspace_root)
            .unwrap_or(&resolved)
            .display()
            .to_string();
        self.attach_context_bytes(
            ContextAttachmentSource::File,
            label,
            Some(display_path),
            bytes,
        )
        .await
    }

    pub async fn detach_context_attachment(
        &self,
        id: &str,
    ) -> squeezy_core::Result<ContextAttachment> {
        let mut state = self.conversation_state.lock().await;
        let Some(index) = state
            .context_attachments
            .iter()
            .position(|attachment| attachment.id == id && attachment.is_active())
        else {
            return Err(SqueezyError::Agent(format!(
                "attachment {id} is not active"
            )));
        };
        state.context_attachments[index].status = ContextAttachmentStatus::Removed;
        let attachment = state.context_attachments[index].clone();
        self.persist_context_attachments(&state)?;
        if let Some(session) = &self.session_log {
            let _ = session.write_context_attachment(&attachment, None);
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_removed",
            None,
            Some(format!("removed {}", attachment.id)),
            json!({ "attachment": attachment.clone() }),
        );
        Ok(attachment)
    }

    pub async fn context_attachments_snapshot(&self) -> Vec<ContextAttachment> {
        self.conversation_state
            .lock()
            .await
            .context_attachments
            .iter()
            .filter(|attachment| attachment.is_active())
            .cloned()
            .collect()
    }

    async fn attach_context_bytes(
        &self,
        source: ContextAttachmentSource,
        label: String,
        path: Option<String>,
        bytes: Vec<u8>,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        let original_sha256 = sha256_hex(&bytes);
        let original_bytes = bytes.len();
        let text = std::str::from_utf8(&bytes).ok();
        let kind = detect_context_attachment_kind(Some(&label), &bytes, text);

        let mut state = self.conversation_state.lock().await;
        if let Some(existing) = state
            .context_attachments
            .iter()
            .find(|attachment| {
                attachment.original_sha256 == original_sha256 && attachment.is_active()
            })
            .cloned()
        {
            drop(state);
            log_session_event(
                self.session_log.as_ref(),
                &self.redactor,
                "context_deduped",
                None,
                Some(format!("deduped {}", existing.id)),
                json!({ "attachment": existing.clone() }),
            );
            return Ok(ContextAttachmentUpdate {
                attachment: existing,
                duplicate: true,
                active: true,
            });
        }

        let id = self.next_context_attachment_id();
        let redacted_label = self.redactor.redact(&label).text;
        let redacted_path = path.map(|value| self.redactor.redact(&value).text);
        if !kind.is_supported_text() {
            let attachment = ContextAttachment {
                id,
                source,
                kind,
                status: ContextAttachmentStatus::Unsupported,
                label: redacted_label,
                path: redacted_path,
                original_sha256,
                redacted_sha256: None,
                original_bytes,
                stored_bytes: 0,
                preview_bytes: 0,
                redactions: 0,
                preview: String::new(),
                truncated: false,
            };
            state.context_attachments.push(attachment.clone());
            self.persist_context_attachments(&state)?;
            if let Some(session) = &self.session_log {
                let _ = session.write_context_attachment(&attachment, None);
            }
            drop(state);
            log_session_event(
                self.session_log.as_ref(),
                &self.redactor,
                "context_unsupported",
                None,
                Some(format!("unsupported {}", attachment.id)),
                json!({ "attachment": attachment.clone() }),
            );
            return Ok(ContextAttachmentUpdate {
                attachment,
                duplicate: false,
                active: false,
            });
        }

        let text = text.unwrap_or_default();
        let (bounded_text, truncated) =
            context_attachment_storage_text(text, DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES);
        let redacted = self.redactor.redact(&bounded_text);
        let (preview, _) =
            context_attachment_preview(&redacted.text, self.config.tool_preview_bytes);
        let attachment = ContextAttachment {
            id,
            source,
            kind,
            status: ContextAttachmentStatus::Attached,
            label: redacted_label,
            path: redacted_path,
            original_sha256,
            redacted_sha256: Some(sha256_hex(redacted.text.as_bytes())),
            original_bytes,
            stored_bytes: redacted.text.len(),
            preview_bytes: preview.len(),
            redactions: redacted.redactions,
            preview,
            truncated,
        };
        state.redactions += attachment.redactions;
        state.context_attachments.push(attachment.clone());
        self.persist_context_attachments(&state)?;
        if let Some(session) = &self.session_log {
            let _ = session.write_context_attachment(&attachment, Some(&redacted.text));
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_attached",
            None,
            Some(format!("attached {}", attachment.id)),
            json!({ "attachment": attachment.clone() }),
        );
        Ok(ContextAttachmentUpdate {
            attachment,
            duplicate: false,
            active: true,
        })
    }

    fn persist_context_attachments(&self, state: &ConversationState) -> squeezy_core::Result<()> {
        // Only persist resume state here. `metadata.resume_available` is set
        // to `true` at session start and `metadata.redactions` is re-synced
        // by `persist_turn_state` on the next completed turn, so we avoid
        // the redundant read-modify-write of `metadata.json` (which also
        // keeps the session_id-bearing metadata out of the attachment flow
        // for static analyzers).
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        Ok(())
    }

    fn next_context_attachment_id(&self) -> String {
        let next = self.next_attachment_id.fetch_add(1, Ordering::Relaxed);
        format!("att-{next:04}")
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
        let telemetry = self.telemetry.clone();
        let redactor = self.redactor.clone();
        let session_metrics = self.session_metrics.clone();
        let all_tool_specs = tools.specs().into_iter().map(advertised_tool).collect();
        let turn_id = TurnId::new(self.next_turn_id.fetch_add(1, Ordering::Relaxed));
        let approval_ids = self.next_approval_id.clone();
        let session_rules = self.session_rules.clone();
        let session_mode = self.session_mode.clone();
        let session_log = self.session_log.clone();
        let conversation_state = self.conversation_state.clone();
        let store = self.store.clone();

        tokio::spawn(async move {
            let redacted_input = redactor.redact(&input);
            let failure_session_log = session_log.clone();
            if tx
                .send(AgentEvent::UserMessage {
                    turn_id,
                    message: TranscriptItem::user(redacted_input.text.clone()),
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
                telemetry: telemetry.clone(),
                redactor: redactor.clone(),
                session_metrics,
                all_tool_specs,
                tx: tx.clone(),
                cancel,
                approval_ids,
                seed_redactions: redacted_input.redactions,
                session_rules,
                session_mode,
                session_log,
                conversation_state,
                store,
            }
            .run(redacted_input.text)
            .await;

            if let Err(error) = outcome {
                let error = redact_error(error, &redactor);
                if let Some(session) = failure_session_log {
                    let _ = session.append_event(SessionEvent::new(
                        "failed",
                        Some(turn_id.to_string()),
                        Some(error.to_string()),
                        json!({ "error": error.to_string() }),
                    ));
                    let _ = session.update_metadata(|metadata| {
                        metadata.status = SessionStatus::Failed;
                        metadata.latest_summary = Some(error.to_string());
                    });
                }
                telemetry.spawn(TelemetryEvent::failure_seen(error_kind(&error)));
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
    telemetry: TelemetryClient,
    redactor: Arc<Redactor>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    all_tool_specs: Vec<AdvertisedTool>,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    // Redactions that already happened on the raw user input before the
    // turn loop began; folded into the first round's metrics so the
    // session metric never undercounts user-side scrubbing.
    seed_redactions: u64,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    store: Option<Arc<SqueezyStore>>,
}

impl TurnRuntime {
    async fn run(mut self, input: String) -> squeezy_core::Result<()> {
        let activation = self.tools.activate_skills_for_input(&input)?;
        let raw_instructions = match self.tools.format_active_skills(&activation.skills) {
            Some(skills) => format!("{}\n\n{}", self.config.instructions, skills),
            None => self.config.instructions.clone(),
        };
        let mut prior_state = self.conversation_state.lock().await.clone();
        let active_attachments = prior_state
            .context_attachments
            .iter()
            .filter(|attachment| attachment.is_active())
            .cloned()
            .collect::<Vec<_>>();
        let user_transcript =
            TranscriptItem::user(format_user_text_with_context(&input, &active_attachments));
        let user_item = LlmInputItem::UserText(format_user_text_with_context(
            &activation.task_input,
            &active_attachments,
        ));
        let mut conversation = prior_state.conversation.clone();
        conversation.push(user_item.clone());
        let mut previous_response_id = if self.config.store_responses {
            prior_state.previous_response_id.take()
        } else {
            None
        };
        let mut next_input = if previous_response_id.is_some() && self.config.store_responses {
            vec![user_item.clone()]
        } else {
            conversation.clone()
        };
        let mut total_cost = CostSnapshot::default();
        let mut seen_tool_outputs = SeenToolOutputs::from_store(self.store.clone());
        let mut broker = CostBroker::new(&self.config);
        broker.metrics.redactions += std::mem::take(&mut self.seed_redactions);
        // Instructions are static across the turn's tool rounds; redact
        // them once so the cost is not paid (or double-counted) per round.
        let redacted_instructions = self.redactor.redact(&raw_instructions);
        broker.metrics.redactions += redacted_instructions.redactions;
        let request_instructions = redacted_instructions.text;
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

        for _round in 0..MAX_TOOL_ROUNDS {
            let active_mode = load_session_mode(&self.session_mode);
            let request = LlmRequest {
                model: self.config.model.clone(),
                instructions: request_instructions.clone(),
                input: redact_llm_input_items(&next_input, &self.redactor),
                max_output_tokens: self.config.max_output_tokens,
                previous_response_id: previous_response_id.clone(),
                tools: advertised_tool_specs(&self.all_tool_specs, active_mode),
                store: self.config.store_responses,
            };
            let request_model = request.model.clone();
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
                        let chunk = assistant_stream.push(&delta);
                        if chunk.text.is_empty() {
                            continue;
                        }
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
                    Ok(LlmEvent::ToolCall(tool_call)) => {
                        let call = ToolCall {
                            call_id: tool_call.call_id,
                            name: tool_call.name,
                            arguments: tool_call.arguments,
                        };
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
                    Ok(LlmEvent::Completed {
                        response_id: id,
                        mut cost,
                    }) => {
                        if cost.estimated_usd_micros.is_none() {
                            cost.estimated_usd_micros =
                                estimate_cost(self.provider.name(), &request_model, &cost);
                        }
                        broker.metrics.record_provider(&cost);
                        merge_cost(&mut total_cost, &cost);
                        response_id = id;
                        completed = true;
                        break;
                    }
                    Ok(LlmEvent::Cancelled) => {
                        // A cancelled turn leaves the session active so the
                        // user can keep working. The lifecycle status only
                        // flips when the agent itself is finalized (TUI exit
                        // or `finish_session`). Recording the event here
                        // still makes the cancellation discoverable in
                        // `events.jsonl` and `squeezy sessions show`.
                        self.log_event(
                            "cancelled",
                            Some(self.turn_id),
                            Some("turn cancelled".to_string()),
                            json!({}),
                        );
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
                self.flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await;
                broker.metrics.redactions += assistant_stream.total_redactions();
                let message = TranscriptItem::assistant(std::mem::take(&mut assistant_message));
                conversation.push(LlmInputItem::AssistantText(message.content.clone()));
                self.persist_turn_state(
                    &conversation,
                    previous_response_id.clone(),
                    user_transcript.clone(),
                    message.clone(),
                    &total_cost,
                    &broker.metrics,
                )
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id: None,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }

            if tool_calls.is_empty() {
                self.flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await;
                broker.metrics.redactions += assistant_stream.total_redactions();
                let message = TranscriptItem::assistant(std::mem::take(&mut assistant_message));
                conversation.push(LlmInputItem::AssistantText(message.content.clone()));
                self.persist_turn_state(
                    &conversation,
                    response_id.clone(),
                    user_transcript.clone(),
                    message.clone(),
                    &total_cost,
                    &broker.metrics,
                )
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }

            let results = execute_tool_calls(
                tool_calls.clone(),
                ToolExecutionContext {
                    turn_id: self.turn_id,
                    provider: self.provider.clone(),
                    tools: &self.tools,
                    config: &self.config,
                    telemetry: self.telemetry.clone(),
                    redactor: self.redactor.clone(),
                    tx: self.tx.clone(),
                    cancel: self.cancel.clone(),
                    approval_ids: self.approval_ids.clone(),
                    session_rules: self.session_rules.clone(),
                    session_mode: self.session_mode.clone(),
                    session_log: self.session_log.clone(),
                },
                &mut broker,
            )
            .await;
            let results = seen_tool_outputs.prepare_results(results);
            let results = pack_tool_results(results, self.config.max_tool_result_bytes_per_round);
            for pending in &results {
                broker.record_model_result(&pending.result);
            }
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

            if self.config.store_responses {
                previous_response_id = response_id;
                next_input = outputs;
            } else {
                previous_response_id = None;
                next_input = conversation.clone();
            }
        }

        Err(SqueezyError::Agent(format!(
            "stopped after {MAX_TOOL_ROUNDS} tool rounds"
        )))
    }

    async fn finish_turn(&self, metrics: &TurnMetrics) {
        self.telemetry.spawn(TelemetryEvent::turn_completed(
            &self.config,
            self.turn_id.get(),
            metrics.clone(),
        ));
        self.session_metrics.lock().await.merge_turn(metrics);
    }

    async fn persist_turn_state(
        &self,
        conversation: &[LlmInputItem],
        response_id: Option<String>,
        user: TranscriptItem,
        assistant: TranscriptItem,
        cost: &CostSnapshot,
        metrics: &TurnMetrics,
    ) {
        let mut state = self.conversation_state.lock().await;
        state.conversation = conversation.to_vec();
        state.previous_response_id = if self.config.store_responses {
            response_id.clone()
        } else {
            None
        };
        state.transcript.push(user);
        state.transcript.push(assistant.clone());
        merge_cost(&mut state.cost, cost);
        state.metrics.merge_turn(metrics);
        state.redactions += metrics.redactions;
        if let Some(session) = &self.session_log {
            let _ = session.write_resume_state(&state.to_resume_state());
            let _ = session.update_metadata(|metadata| {
                metadata.cost = state.cost.clone();
                metadata.metrics = state.metrics.clone();
                metadata.redactions = state.redactions;
                metadata.resume_available = true;
                metadata.mode = load_session_mode(&self.session_mode);
            });
        }
        drop(state);
        self.log_event(
            "assistant_completed",
            Some(self.turn_id),
            Some(assistant.content),
            json!({
                "response_id": response_id,
                "cost": cost,
                "metrics": metrics,
            }),
        );
    }

    fn log_event(
        &self,
        kind: &str,
        turn_id: Option<TurnId>,
        summary: Option<String>,
        payload: Value,
    ) {
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            kind,
            turn_id,
            summary,
            payload,
        );
    }

    /// Flushes any text the stream redactor is still holding behind its
    /// tail buffer, emitting it as a final AssistantDelta and appending
    /// it to the running message accumulator. Idempotent on an already
    /// flushed stream.
    async fn flush_assistant_stream(
        &self,
        assistant_stream: &mut StreamRedactor,
        assistant_message: &mut String,
    ) {
        let tail = assistant_stream.finish();
        if tail.text.is_empty() {
            return;
        }
        assistant_message.push_str(&tail.text);
        let _ = self
            .tx
            .send(AgentEvent::AssistantDelta {
                turn_id: self.turn_id,
                delta: tail.text,
            })
            .await;
    }
}

#[derive(Clone)]
struct ToolExecutionContext<'a> {
    turn_id: TurnId,
    provider: Arc<dyn LlmProvider>,
    tools: &'a ToolRegistry,
    config: &'a AppConfig,
    telemetry: TelemetryClient,
    redactor: Arc<Redactor>,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
}

async fn execute_tool_calls(
    calls: Vec<ToolCall>,
    context: ToolExecutionContext<'_>,
    broker: &mut CostBroker,
) -> Vec<ToolResult> {
    let mut approved = Vec::new();
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
    let mut recorded = vec![false; calls.len()];

    for (index, call) in calls.iter().enumerate() {
        let tool_sequence = match broker.reserve_call() {
            Ok(tool_sequence) => tool_sequence,
            Err((tool_sequence, reason)) => {
                let result = budget_denied_result(call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
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

        match permission_decision(call, &context).await {
            ApprovalDecision::Approved => approved.push((index, call.clone(), tool_sequence)),
            ApprovalDecision::Denied(reason) => {
                let result = ToolResult::denied(call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
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
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
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

    let mut parallel_batch = Vec::new();
    for (index, call, tool_sequence) in approved {
        if context.tools.is_parallel_safe(&call) {
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
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
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            let result = run_one_tool(context.clone(), tool_sequence, call).await;
            broker.record_executed_result(&result);
            results[index] = Some(result);
            recorded[index] = true;
        }
    }
    flush_parallel_batch(&context, broker, &mut results, &mut parallel_batch).await;

    collect_recorded_results(
        results,
        recorded,
        broker,
        context.config,
        &context.telemetry,
    )
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
    if broker.enforces_result_budgets() {
        for (index, call, tool_sequence) in calls {
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                continue;
            }
            let result = run_one_tool(context.clone(), tool_sequence, call).await;
            broker.record_executed_result(&result);
            results[index] = Some(result);
        }
        return;
    }

    let completions =
        futures_util::stream::iter(calls.into_iter().map(|(index, call, tool_sequence)| {
            let context = context.clone();
            async move {
                let result = run_one_tool(context, tool_sequence, call).await;
                (index, result)
            }
        }))
        .buffer_unordered(context.config.max_parallel_tools.max(1))
        .collect::<Vec<_>>()
        .await;

    for (index, result) in completions {
        broker.record_executed_result(&result);
        results[index] = Some(result);
    }
}

async fn run_one_tool(
    context: ToolExecutionContext<'_>,
    tool_sequence: u64,
    call: ToolCall,
) -> ToolResult {
    let _ = context
        .tx
        .send(AgentEvent::ToolCallStarted {
            turn_id: context.turn_id,
            call: redact_tool_call(call.clone(), &context.redactor),
        })
        .await;
    let started = Instant::now();
    let result = context
        .tools
        .execute_for_group(call, context.cancel.clone(), context.turn_id.to_string())
        .await;
    emit_tool_telemetry(
        context.config,
        &context.telemetry,
        context.turn_id,
        tool_sequence,
        &result,
        started.elapsed(),
    );
    let _ = context
        .tx
        .send(AgentEvent::ToolCallCompleted {
            turn_id: context.turn_id,
            result: result.clone(),
        })
        .await;
    result
}

#[derive(Debug)]
struct CostBroker {
    max_tool_calls: u64,
    max_bytes_read: u64,
    max_search_files: u64,
    metrics: TurnMetrics,
}

impl CostBroker {
    fn new(config: &AppConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls_per_turn,
            max_bytes_read: config.max_tool_bytes_read_per_turn,
            max_search_files: config.max_search_files_per_turn,
            metrics: TurnMetrics::default(),
        }
    }

    fn reserve_call(&mut self) -> Result<u64, (u64, String)> {
        self.metrics.tool_calls += 1;
        let tool_sequence = self.metrics.tool_calls;
        if tool_sequence > self.max_tool_calls {
            Err((
                tool_sequence,
                format!(
                    "per-turn tool-call budget exceeded: limit={}",
                    self.max_tool_calls
                ),
            ))
        } else {
            Ok(tool_sequence)
        }
    }

    fn deny_reason(&self) -> Option<String> {
        if self.metrics.bytes_read >= self.max_bytes_read {
            Some(format!(
                "per-turn tool byte-read budget exceeded: limit={}",
                self.max_bytes_read
            ))
        } else if self.metrics.files_scanned >= self.max_search_files {
            Some(format!(
                "per-turn search file-scan budget exceeded: limit={}",
                self.max_search_files
            ))
        } else {
            None
        }
    }

    fn enforces_result_budgets(&self) -> bool {
        self.max_bytes_read < u64::MAX || self.max_search_files < u64::MAX
    }

    fn record_executed_result(&mut self, result: &ToolResult) {
        match result.status {
            ToolStatus::Success => self.metrics.tool_successes += 1,
            ToolStatus::Error | ToolStatus::Stale => self.metrics.tool_errors += 1,
            ToolStatus::Denied => self.metrics.tool_denials += 1,
            ToolStatus::Cancelled => self.metrics.tool_cancellations += 1,
        }
        self.metrics.files_scanned += result.cost_hint.files_scanned;
        self.metrics.bytes_read += result.cost_hint.bytes_read;
        self.metrics.matches_returned += result.cost_hint.matches_returned;
        self.metrics.redactions += result.cost_hint.redactions;
        if result.content.get("spilled").and_then(Value::as_bool) == Some(true) {
            self.metrics.spill_writes += 1;
        }
        if result.tool_name == "read_tool_output" && result.status == ToolStatus::Success {
            self.metrics.spill_reads += 1;
        }
        if is_budget_denied(result) {
            self.metrics.budget_denials += 1;
        }
    }

    fn record_model_result(&mut self, result: &ToolResult) {
        self.metrics.model_output_bytes += result.model_output().len() as u64;
        if result.content.get("receipt_stub").and_then(Value::as_bool) == Some(true) {
            self.metrics.receipt_stub_hits += 1;
        }
        if result
            .content
            .get("negative_receipt_stub")
            .and_then(Value::as_bool)
            == Some(true)
        {
            self.metrics.negative_receipt_hits += 1;
        }
        if is_budget_denied(result) {
            self.metrics.budget_denials += 1;
        }
    }
}

fn budget_denied_result(call: &ToolCall, reason: String) -> ToolResult {
    let content = json!({
        "error": reason,
        "budget_denied": true,
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();
    ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        status: ToolStatus::Denied,
        content,
        cost_hint: ToolCostHint {
            output_bytes: output_bytes.len() as u64,
            truncated: true,
            ..ToolCostHint::default()
        },
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: None,
        },
        spill_model_output: None,
    }
}

fn emit_tool_telemetry(
    config: &AppConfig,
    telemetry: &TelemetryClient,
    turn_id: TurnId,
    tool_sequence: u64,
    result: &ToolResult,
    duration: Duration,
) {
    telemetry.spawn(TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: turn_id.get(),
        tool_sequence,
        tool_name: &result.tool_name,
        status: telemetry_tool_status(result.status),
        duration,
        cost: ToolCostProperties {
            files_scanned: result.cost_hint.files_scanned,
            bytes_read: result.cost_hint.bytes_read,
            matches_returned: result.cost_hint.matches_returned,
            output_bytes: result.cost_hint.output_bytes,
        },
    }));
}

fn telemetry_tool_status(status: ToolStatus) -> TelemetryToolStatusKind {
    match status {
        ToolStatus::Success => TelemetryToolStatusKind::Success,
        ToolStatus::Error => TelemetryToolStatusKind::Error,
        ToolStatus::Denied => TelemetryToolStatusKind::Denied,
        ToolStatus::Stale => TelemetryToolStatusKind::Stale,
        ToolStatus::Cancelled => TelemetryToolStatusKind::Cancelled,
    }
}

fn is_budget_denied(result: &ToolResult) -> bool {
    result.content.get("budget_denied").and_then(Value::as_bool) == Some(true)
}

fn error_kind(error: &SqueezyError) -> ErrorKind {
    match error {
        SqueezyError::ProviderNotConfigured(_)
        | SqueezyError::ProviderRequest(_)
        | SqueezyError::ProviderStream(_) => ErrorKind::Provider,
        SqueezyError::Tool(_) => ErrorKind::Tool,
        SqueezyError::Permission(_) => ErrorKind::Permission,
        SqueezyError::Graph(_) => ErrorKind::Graph,
        SqueezyError::Io(_) => ErrorKind::Io,
        SqueezyError::Config(_) => ErrorKind::Config,
        SqueezyError::Agent(_)
        | SqueezyError::Terminal(_)
        | SqueezyError::Workspace(_)
        | SqueezyError::Parse(_) => ErrorKind::Unknown,
    }
}

async fn permission_decision(
    call: &ToolCall,
    context: &ToolExecutionContext<'_>,
) -> ApprovalDecision {
    let request = context.tools.permission_request(call);
    let active_mode = load_session_mode(&context.session_mode);
    if let Some(verdict) = mode_permission_verdict(active_mode, &request) {
        log_permission_verdict(&request, &verdict);
        return ApprovalDecision::Denied(context.redactor.redact(&verdict.reason).text);
    }
    let session_rules = snapshot_session_rules(&context.session_rules);
    let mut verdict = context
        .config
        .permissions
        .evaluate_with_extra(&request, &session_rules);
    if should_classify_shell(context.config, context.provider.name(), &request, &verdict)
        && let Some(classifier) = classify_ambiguous_shell(
            context.provider.clone(),
            context.config,
            &request,
            context.cancel.clone(),
        )
        .await
    {
        verdict = classifier;
    }
    log_permission_verdict(&request, &verdict);
    match verdict.action {
        PermissionAction::Allow => ApprovalDecision::Approved,
        PermissionAction::Deny => {
            ApprovalDecision::Denied(context.redactor.redact(&verdict.reason).text)
        }
        PermissionAction::Ask => {
            let (decision_tx, decision_rx) = oneshot::channel();
            let approval_request = ToolApprovalRequest {
                id: context.approval_ids.fetch_add(1, Ordering::Relaxed),
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                scope: legacy_scope_for_capability(request.capability),
                permission: redact_permission_request(request.clone(), &context.redactor),
                matched_rule: verdict.matched_rule,
                reason: context.redactor.redact(&verdict.reason).text,
            };
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "approval_requested",
                Some(context.turn_id),
                Some(call.name.clone()),
                json!({
                    "tool": call.name,
                    "call_id": call.call_id,
                    "permission": approval_request.permission,
                    "reason": approval_request.reason,
                }),
            );
            let send_approval = context.tx.send(AgentEvent::ApprovalRequested {
                turn_id: context.turn_id,
                request: approval_request,
                decision_tx,
            });
            let send_result = tokio::select! {
                _ = context.cancel.cancelled() => return ApprovalDecision::Cancelled,
                result = send_approval => result,
            };
            if send_result.is_err() {
                return ApprovalDecision::Denied("approval channel closed".to_string());
            }
            let decision = tokio::select! {
                _ = context.cancel.cancelled() => return ApprovalDecision::Cancelled,
                decision = decision_rx => decision,
            };
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "approval_decided",
                Some(context.turn_id),
                Some(format!("{decision:?}")),
                json!({ "decision": format!("{decision:?}") }),
            );
            match decision {
                Ok(ToolApprovalDecision::Approved | ToolApprovalDecision::AllowOnce) => {
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AllowRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Allow,
                    );
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AllowRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Allow,
                    );
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AskRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Ask,
                    );
                    ApprovalDecision::Denied(
                        "user asked to require approval for future matching calls".to_string(),
                    )
                }
                Ok(ToolApprovalDecision::AskRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Ask,
                    );
                    ApprovalDecision::Denied(
                        "user asked to require approval for future matching calls".to_string(),
                    )
                }
                Ok(ToolApprovalDecision::Denied | ToolApprovalDecision::DenyOnce) => {
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied tool call",
                    ))
                }
                Ok(ToolApprovalDecision::DenyRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Deny,
                    );
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied and persisted a user rule",
                    ))
                }
                Ok(ToolApprovalDecision::DenyRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Deny,
                    );
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied and persisted a project rule",
                    ))
                }
                Err(_) => ApprovalDecision::Denied("approval was not answered".to_string()),
            }
        }
    }
}

/// Lock-free read of the active session mode. Defaults to `Build` if the
/// stored byte is corrupted, but that path is unreachable in normal flow
/// because every writer goes through `SessionMode::to_u8`.
fn load_session_mode(session_mode: &Arc<AtomicU8>) -> SessionMode {
    let raw = session_mode.load(Ordering::Acquire);
    SessionMode::from_u8(raw).unwrap_or_else(|| {
        tracing::warn!(
            target: "squeezy::permissions",
            discriminant = raw,
            "unexpected session mode discriminant; defaulting to build",
        );
        SessionMode::Build
    })
}

pub(crate) fn mode_permission_verdict(
    mode: SessionMode,
    request: &PermissionRequest,
) -> Option<PermissionVerdict> {
    if !mode_refuses_capability(mode, request.capability) {
        return None;
    }
    Some(PermissionVerdict {
        action: PermissionAction::Deny,
        matched_rule: None,
        reason: format!(
            "{} mode refuses {}",
            mode.as_str(),
            request.capability.as_str()
        ),
    })
}

/// Single source of truth for whether a session mode forbids a capability.
/// Plan mode allows only Read and Search; Build mode allows everything (the
/// configured `PermissionPolicy` still applies). The capability list is
/// intentionally exhaustive (`match`) so adding a new capability is a
/// compile-time prompt to decide whether plan mode admits it.
fn mode_refuses_capability(mode: SessionMode, capability: PermissionCapability) -> bool {
    if mode == SessionMode::Build {
        return false;
    }
    match capability {
        PermissionCapability::Read | PermissionCapability::Search => false,
        PermissionCapability::Edit
        | PermissionCapability::Shell
        | PermissionCapability::Git
        | PermissionCapability::Network
        | PermissionCapability::Mcp
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => true,
    }
}

fn snapshot_session_rules(session_rules: &Arc<RwLock<Vec<PermissionRule>>>) -> Vec<PermissionRule> {
    session_rules
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_else(|err| {
            tracing::warn!(
                target: "squeezy::permissions",
                error = %err,
                "session permission rule lock was poisoned; treating session rules as empty",
            );
            Vec::new()
        })
}

fn log_session_mode_transition(from_mode: SessionMode, to_mode: SessionMode, source: &'static str) {
    tracing::info!(
        target: "squeezy::permissions",
        from_mode = %from_mode.as_str(),
        to_mode = %to_mode.as_str(),
        source,
        "session mode transition",
    );
}

fn log_permission_verdict(request: &PermissionRequest, verdict: &PermissionVerdict) {
    let matched_source = verdict
        .matched_rule
        .as_ref()
        .map(|rule| rule.source.as_str());
    let matched_target = verdict
        .matched_rule
        .as_ref()
        .map(|rule| rule.target.as_str());
    tracing::info!(
        target: "squeezy::permissions",
        tool = %request.tool_name,
        capability = %request.capability.as_str(),
        target = %request.target,
        risk = %request.risk.as_str(),
        action = %verdict.action.as_str(),
        matched_source,
        matched_target,
        reason = %verdict.reason,
        "permission verdict",
    );
}

fn should_classify_shell(
    config: &AppConfig,
    provider_name: &str,
    request: &PermissionRequest,
    verdict: &PermissionVerdict,
) -> bool {
    config.permissions.shell_classifier
        && provider_name != "scripted"
        && request.tool_name == "shell"
        && request.capability == PermissionCapability::Shell
        && verdict.action == PermissionAction::Ask
}

async fn classify_ambiguous_shell(
    provider: Arc<dyn LlmProvider>,
    config: &AppConfig,
    request: &PermissionRequest,
    cancel: CancellationToken,
) -> Option<PermissionVerdict> {
    let command = request.metadata.get("command")?;
    let prompt = format!(
        "Classify this shell command for a coding agent permission gate.\n\
Return compact JSON with fields {{\"action\": \"ask\"|\"deny\", \"reason\": string}}.\n\
Never return allow.\n\
Command: {command:?}\n\
Working target: {:?}",
        request.target
    );
    let llm_request = LlmRequest {
        model: config.model.clone(),
        instructions: "You classify shell-command risk for a local coding agent. Return JSON only."
            .to_string(),
        input: vec![LlmInputItem::UserText(prompt)],
        max_output_tokens: Some(80),
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };
    let mut stream = provider.stream_response(llm_request, cancel);
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event.ok()? {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::Completed { .. } => break,
            LlmEvent::Cancelled => return None,
            LlmEvent::Started | LlmEvent::ToolCall(_) => {}
        }
    }
    Some(parse_classifier_verdict(&text))
}

/// Parse the classifier's textual response into a verdict. Only `deny` can
/// flip the verdict; missing or unparseable output leaves the call as `ask`.
/// Made `pub(crate)` so tests can exercise the JSON parsing rules.
pub(crate) fn parse_classifier_verdict(text: &str) -> PermissionVerdict {
    let trimmed = text.trim();
    let action = extract_json_action(trimmed)
        .or_else(|| extract_loose_action(trimmed))
        .unwrap_or(PermissionAction::Ask);
    let reason_excerpt = compact_reason(trimmed);
    match action {
        PermissionAction::Deny => PermissionVerdict {
            action: PermissionAction::Deny,
            matched_rule: None,
            reason: format!("shell classifier denied command: {reason_excerpt}"),
        },
        // Allow from the classifier is intentionally disallowed - we keep the
        // verdict at Ask so a human still confirms.
        _ => PermissionVerdict {
            action: PermissionAction::Ask,
            matched_rule: None,
            reason: format!("shell classifier requires approval: {reason_excerpt}"),
        },
    }
}

fn extract_json_action(text: &str) -> Option<PermissionAction> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let candidate = &text[start..=end];
    let value: serde_json::Value = serde_json::from_str(candidate).ok()?;
    let action = value.get("action")?.as_str()?;
    match action.trim().to_ascii_lowercase().as_str() {
        "deny" | "denied" | "refuse" => Some(PermissionAction::Deny),
        "ask" | "prompt" | "confirm" => Some(PermissionAction::Ask),
        _ => None,
    }
}

fn extract_loose_action(text: &str) -> Option<PermissionAction> {
    // Defensive fallback when the model returns "action: deny" or similar
    // without strict JSON. Look for a colon-bound "action" field and read the
    // next bare word.
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("action")?;
    let after = &lower[idx + "action".len()..];
    let after = after.trim_start_matches(|c: char| !c.is_alphanumeric());
    if after.starts_with("deny") {
        Some(PermissionAction::Deny)
    } else if after.starts_with("ask") {
        Some(PermissionAction::Ask)
    } else {
        None
    }
}

fn compact_reason(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn legacy_scope_for_capability(capability: PermissionCapability) -> PermissionScope {
    match capability {
        PermissionCapability::Read | PermissionCapability::Search => PermissionScope::Read,
        PermissionCapability::Edit => PermissionScope::Edit,
        PermissionCapability::Network => PermissionScope::Web,
        PermissionCapability::Shell
        | PermissionCapability::Mcp
        | PermissionCapability::Git
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => PermissionScope::Shell,
    }
}

fn permission_denied_reason(request: &PermissionRequest, reason: &str) -> String {
    format!(
        "{reason}; capability={} target={} risk={}",
        request.capability.as_str(),
        request.target,
        request.risk.as_str()
    )
}

/// Install a user/project rule both into the in-memory session list and (best
/// effort) on disk. Returns immediately when the rule cannot be persisted; the
/// failure is logged but never bubbled to the caller, since the current call
/// has already been resolved by the approval response.
fn install_persistent_rule(
    context: &ToolExecutionContext<'_>,
    request: &PermissionRequest,
    source: PermissionRuleSource,
    action: PermissionAction,
) {
    let Some(rule) = permission_rule_for_persistence(request, source, action) else {
        tracing::warn!(
            target: "squeezy::permissions",
            capability = %request.capability.as_str(),
            target = %request.target,
            action = %action.as_str(),
            "refused to install permission rule (e.g. Allow on destructive capability)",
        );
        return;
    };

    match context.session_rules.write() {
        Ok(mut guard) => guard.push(rule.clone()),
        Err(err) => {
            tracing::warn!(
                target: "squeezy::permissions",
                error = %err,
                "could not install session permission rule",
            );
        }
    }

    let path = match persistence_path_for(context.config, source) {
        Some(path) => path,
        None => return,
    };
    if let Err(err) = write_permission_rule(&path, &rule) {
        tracing::warn!(
            target: "squeezy::permissions",
            path = %path.display(),
            error = %err,
            "failed to persist permission rule",
        );
    } else {
        tracing::info!(
            target: "squeezy::permissions",
            path = %path.display(),
            capability = %rule.capability,
            target = %rule.target,
            action = %rule.action.as_str(),
            source = %rule.source.as_str(),
            "persisted permission rule",
        );
    }
}

fn persistence_path_for(config: &AppConfig, source: PermissionRuleSource) -> Option<PathBuf> {
    match source {
        PermissionRuleSource::User => Some(default_settings_path()),
        PermissionRuleSource::Project => Some(config.workspace_root.join(PROJECT_SETTINGS_FILE)),
        PermissionRuleSource::Builtin | PermissionRuleSource::Session => None,
    }
}

fn write_permission_rule(path: &std::path::Path, rule: &PermissionRule) -> io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let reason = rule
        .reason
        .clone()
        .unwrap_or_else(|| "added from approval prompt".to_string());
    let mut text = String::new();
    text.push_str("\n[[permissions.rules]]\n");
    text.push_str(&format!(
        "capability = {}\n",
        escape_toml_basic_string(&rule.capability)
    ));
    text.push_str(&format!(
        "target = {}\n",
        escape_toml_basic_string(&rule.target)
    ));
    text.push_str(&format!(
        "action = {}\n",
        escape_toml_basic_string(rule.action.as_str())
    ));
    text.push_str(&format!(
        "source = {}\n",
        escape_toml_basic_string(rule.source.as_str())
    ));
    text.push_str(&format!("reason = {}\n", escape_toml_basic_string(&reason)));
    file.write_all(text.as_bytes())
}

/// Pick a rule shape to persist for this approval. Refuses Allow on any
/// destructive capability (regardless of target), and refuses Allow rules that
/// would broadly match all paths/commands via a `*` target.
pub(crate) fn permission_rule_for_persistence(
    request: &PermissionRequest,
    source: PermissionRuleSource,
    action: PermissionAction,
) -> Option<PermissionRule> {
    let mut rule = request.suggested_rules.first().cloned().unwrap_or_else(|| {
        PermissionRule::new(
            request.capability.as_str(),
            request.target.clone(),
            action,
            source,
            Some("added from approval prompt".to_string()),
        )
    });
    rule.action = action;
    rule.source = source;
    if action == PermissionAction::Allow {
        if request.capability == PermissionCapability::Destructive {
            return None;
        }
        if rule.capability == "destructive" {
            return None;
        }
        if squeezy_core::target_is_effectively_wildcard(&rule.target) {
            return None;
        }
    }
    Some(rule)
}

/// Pair of an LLM-facing tool spec and the capability used to decide whether
/// the tool is advertised in a given session mode. Carrying the capability
/// alongside the spec keeps the advertisement filter in lock-step with the
/// per-call permission decision: both consult the same enum, and the source
/// of truth lives in `squeezy-tools` next to each tool's builder.
#[derive(Clone)]
pub(crate) struct AdvertisedTool {
    spec: LlmToolSpec,
    capability: PermissionCapability,
}

pub(crate) fn advertised_tool(spec: ToolSpec) -> AdvertisedTool {
    AdvertisedTool {
        capability: spec.capability,
        spec: LlmToolSpec {
            name: spec.name,
            description: spec.description,
            parameters: spec.parameters,
            strict: false,
        },
    }
}

fn advertised_tool_specs(tools: &[AdvertisedTool], mode: SessionMode) -> Vec<LlmToolSpec> {
    tools
        .iter()
        .filter(|tool| !mode_refuses_capability(mode, tool.capability))
        .map(|tool| tool.spec.clone())
        .collect()
}

fn llm_function_call_item(call: ToolCall, redactor: &Redactor) -> LlmInputItem {
    LlmInputItem::FunctionCall {
        call_id: call.call_id,
        name: call.name,
        arguments: redact_json_value(call.arguments, redactor),
    }
}

fn redact_llm_input_items(input: &[LlmInputItem], redactor: &Redactor) -> Vec<LlmInputItem> {
    input
        .iter()
        .cloned()
        .map(|item| match item {
            LlmInputItem::UserText(text) => LlmInputItem::UserText(redactor.redact(&text).text),
            LlmInputItem::AssistantText(text) => {
                LlmInputItem::AssistantText(redactor.redact(&text).text)
            }
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments: redact_json_value(arguments, redactor),
            },
            LlmInputItem::FunctionCallOutput { call_id, output } => {
                LlmInputItem::FunctionCallOutput {
                    call_id,
                    output: redactor.redact(&output).text,
                }
            }
        })
        .collect()
}

/// Scrub the user/UI-facing surfaces of a `PermissionRequest` so an approval
/// prompt cannot leak a secret that appeared in a shell command, file path,
/// or rule metadata. Capability and risk are enum-only and need no redaction.
fn redact_permission_request(
    mut request: PermissionRequest,
    redactor: &Redactor,
) -> PermissionRequest {
    request.target = redactor.redact(&request.target).text;
    request.summary = redactor.redact(&request.summary).text;
    request.metadata = request
        .metadata
        .into_iter()
        .map(|(key, value)| (key, redactor.redact(&value).text))
        .collect();
    request
}

fn redact_tool_call(mut call: ToolCall, redactor: &Redactor) -> ToolCall {
    call.arguments = redact_json_value(call.arguments, redactor);
    call
}

fn redact_json_value(value: Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(text) => Value::String(redactor.redact(&text).text),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_value(item, redactor))
                .collect(),
        ),
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, redact_json_value(value, redactor)))
                .collect(),
        ),
        value => value,
    }
}

fn redact_error(error: SqueezyError, redactor: &Redactor) -> SqueezyError {
    match error {
        SqueezyError::Config(message) => SqueezyError::Config(redactor.redact(&message).text),
        SqueezyError::ProviderNotConfigured(message) => {
            SqueezyError::ProviderNotConfigured(redactor.redact(&message).text)
        }
        SqueezyError::ProviderRequest(message) => {
            SqueezyError::ProviderRequest(redactor.redact(&message).text)
        }
        SqueezyError::ProviderStream(message) => {
            SqueezyError::ProviderStream(redactor.redact(&message).text)
        }
        SqueezyError::Terminal(message) => SqueezyError::Terminal(redactor.redact(&message).text),
        SqueezyError::Agent(message) => SqueezyError::Agent(redactor.redact(&message).text),
        SqueezyError::Workspace(message) => SqueezyError::Workspace(redactor.redact(&message).text),
        SqueezyError::Parse(message) => SqueezyError::Parse(redactor.redact(&message).text),
        SqueezyError::Graph(message) => SqueezyError::Graph(redactor.redact(&message).text),
        SqueezyError::Tool(message) => SqueezyError::Tool(redactor.redact(&message).text),
        SqueezyError::Permission(message) => {
            SqueezyError::Permission(redactor.redact(&message).text)
        }
        SqueezyError::Io(error) => SqueezyError::Io(error),
    }
}

fn merge_cost(total: &mut CostSnapshot, next: &CostSnapshot) {
    total.input_tokens = add_optional(total.input_tokens, next.input_tokens);
    total.output_tokens = add_optional(total.output_tokens, next.output_tokens);
    total.cached_input_tokens = add_optional(total.cached_input_tokens, next.cached_input_tokens);
    total.cache_write_input_tokens = add_optional(
        total.cache_write_input_tokens,
        next.cache_write_input_tokens,
    );
    total.estimated_usd_micros =
        add_optional(total.estimated_usd_micros, next.estimated_usd_micros);
}

fn start_session_log(config: &AppConfig, provider: &str) -> Option<SessionHandle> {
    let store = SessionStore::open(config);
    let metadata = SessionMetadata::new(config, provider);
    match store.start_session(metadata) {
        Ok(handle) => {
            let _ = handle.append_event(SessionEvent::new(
                "session_started",
                None,
                Some("session started".to_string()),
                json!({}),
            ));
            Some(handle)
        }
        Err(error) => {
            tracing::warn!(
                target: "squeezy::sessions",
                %error,
                "session logging disabled for this run",
            );
            None
        }
    }
}

fn next_attachment_counter(attachments: &[ContextAttachment]) -> u64 {
    attachments
        .iter()
        .filter_map(|attachment| attachment.id.strip_prefix("att-"))
        .filter_map(|suffix| suffix.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn format_user_text_with_context(input: &str, attachments: &[ContextAttachment]) -> String {
    if attachments.is_empty() {
        return input.to_string();
    }
    let mut output = input.to_string();
    output.push_str("\n\nAttached context references:\n");
    for attachment in attachments {
        output.push_str(&format!(
            "- {reference} id={id} source={source} kind={kind} label={label:?} bytes={bytes} stored_bytes={stored_bytes} truncated={truncated}\n",
            reference = attachment.reference(),
            id = attachment.id,
            source = attachment.source.as_str(),
            kind = attachment.kind.as_str(),
            label = attachment.label,
            bytes = attachment.original_bytes,
            stored_bytes = attachment.stored_bytes,
            truncated = attachment.truncated,
        ));
        if let Some(path) = &attachment.path {
            output.push_str(&format!("  path={path:?}\n"));
        }
        if !attachment.preview.is_empty() {
            output.push_str("  redacted_preview:\n");
            for line in attachment.preview.lines().take(20) {
                output.push_str("    ");
                output.push_str(line);
                output.push('\n');
            }
        }
    }
    output
}

fn redact_json_payload(payload: Value, redactor: &Redactor) -> Value {
    match payload {
        Value::String(text) => Value::String(redactor.redact(&text).text),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_payload(item, redactor))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_json_payload(value, redactor)))
                .collect(),
        ),
        // Numbers, booleans, and null cannot contain redactable text and we
        // intentionally do not touch JSON object keys so the resulting value
        // keeps a stable shape for callers that index into the payload.
        other => other,
    }
}

fn log_session_event(
    session: Option<&SessionHandle>,
    redactor: &Redactor,
    kind: &str,
    turn_id: Option<TurnId>,
    summary: Option<String>,
    payload: Value,
) {
    let Some(session) = session else {
        return;
    };
    let summary = summary.map(|value| redactor.redact(&value).text);
    let payload = redact_json_payload(payload, redactor);
    let _ = session.append_event(SessionEvent::new(
        kind,
        turn_id.map(|value| value.to_string()),
        summary,
        payload,
    ));
}

fn user_item_summary(item: &LlmInputItem) -> Option<String> {
    match item {
        LlmInputItem::UserText(text) => Some(text.clone()),
        _ => None,
    }
}

fn tool_output_summary(item: &LlmInputItem) -> Option<String> {
    match item {
        LlmInputItem::FunctionCallOutput { call_id, .. } => Some(format!("tool output {call_id}")),
        _ => None,
    }
}

fn llm_input_to_resume_item(item: LlmInputItem) -> ResumeItem {
    match item {
        LlmInputItem::UserText(text) => ResumeItem::UserText { text },
        LlmInputItem::AssistantText(text) => ResumeItem::AssistantText { text },
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => ResumeItem::FunctionCall {
            call_id,
            name,
            arguments,
        },
        LlmInputItem::FunctionCallOutput { call_id, output } => {
            ResumeItem::FunctionCallOutput { call_id, output }
        }
    }
}

fn resume_item_for_json(item: LlmInputItem) -> Value {
    serde_json::to_value(llm_input_to_resume_item(item))
        .unwrap_or_else(|_| json!({"error": "resume item serialization failed"}))
}

fn resume_item_to_llm_input(item: ResumeItem) -> LlmInputItem {
    match item {
        ResumeItem::UserText { text } => LlmInputItem::UserText(text),
        ResumeItem::AssistantText { text } => LlmInputItem::AssistantText(text),
        ResumeItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        },
        ResumeItem::FunctionCallOutput { call_id, output } => {
            LlmInputItem::FunctionCallOutput { call_id, output }
        }
    }
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
    store: Option<Arc<SqueezyStore>>,
}

impl SeenToolOutputs {
    fn from_store(store: Option<Arc<SqueezyStore>>) -> Self {
        let mut outputs = Self {
            by_tool_output: BTreeMap::new(),
            store,
        };
        if let Some(store) = outputs.store.as_deref()
            && let Ok(receipts) = store.tool_receipts()
        {
            for receipt in receipts {
                let seen = SeenToolOutput {
                    call_id: receipt.call_id,
                    tool_name: receipt.tool_name,
                    stable_output_sha256: receipt.stable_output_sha256,
                    content_sha256: receipt.content_sha256,
                    model_output_bytes: receipt.model_output_bytes,
                };
                outputs
                    .by_tool_output
                    .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                    .or_insert(seen);
            }
        }
        outputs
    }

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
                    .or_insert(seen.clone());
                if let Some(store) = self.store.as_deref() {
                    let _ = store.put_tool_receipt(&StoredToolReceipt {
                        tool_name: seen.tool_name,
                        stable_output_sha256: seen.stable_output_sha256,
                        call_id: seen.call_id,
                        content_sha256: seen.content_sha256,
                        model_output_bytes: seen.model_output_bytes,
                        created_unix_millis: unix_millis(),
                    });
                }
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
    let negative_receipt_stub = is_negative_receipt_result(&result);
    let content = json!({
        "receipt_stub": true,
        "negative_receipt_stub": negative_receipt_stub,
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
        spill_model_output: None,
    }
}

fn is_negative_receipt_result(result: &ToolResult) -> bool {
    match result.tool_name.as_str() {
        "grep" => {
            result
                .content
                .get("matches")
                .and_then(Value::as_array)
                .is_some_and(|items| items.is_empty())
                || result
                    .content
                    .get("paths")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.is_empty())
                || result.content.get("count").and_then(Value::as_u64) == Some(0)
        }
        "glob" => result
            .content
            .get("paths")
            .and_then(Value::as_array)
            .is_some_and(|items| items.is_empty()),
        _ => false,
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
        spill_model_output: None,
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
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
    pub permission: PermissionRequest,
    pub matched_rule: Option<PermissionRule>,
    pub reason: String,
}

impl ToolApprovalRequest {
    pub fn summary(&self) -> &str {
        &self.permission.summary
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approved,
    Denied,
    AllowOnce,
    AllowRuleUser,
    AllowRuleProject,
    AskRuleUser,
    AskRuleProject,
    DenyOnce,
    DenyRuleUser,
    DenyRuleProject,
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
        metrics: TurnMetrics,
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
