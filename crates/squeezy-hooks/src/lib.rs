//! Minimal hook system for Squeezy.
//!
//! Two trait surfaces coexist here:
//!
//! * [`AgentHook`] — typed, mutation-capable async surface for
//!   skills, MCP, telemetry, and any new extension that needs to
//!   intercept the agent loop with structured input. Handlers receive
//!   `&mut` views over the LLM request, tool call, and tool result
//!   payloads and can rewrite them in place. This is the contract new
//!   integrations should target.
//! * [`HookHandler`] — the older synchronous observation-style API
//!   keyed by [`HookEvent`]. Skill hook scripts and the
//!   squeezy-agent dispatch sites still consume it; the
//!   [`LegacyHookForwarder`] bridges it into the typed surface.
//!
//! The observation pipeline drives the agent today: the
//! [`HookRegistry`] is plugged into `squeezy-agent` and fans events
//! out for every variant of [`HookEvent`] from the natural site
//! listed on each variant's doc comment.
//!
//! Payloads use the typed [`HookPayload`] enum so the dispatch site
//! and the handler agree on the shape of every event. Handlers that
//! need a `serde_json::Value` (e.g. skill hook scripts piping the
//! payload to an external command) project to JSON via
//! [`HookContext::payload_json`]. The mutation contract is honored at
//! every site that has a natural mutation target: today
//! [`HookEvent::PreTurn`] handlers may append `extra_instructions` to
//! the per-turn instructions and [`HookEvent::UserPromptSubmit`]
//! handlers may rewrite the raw user prompt; other sites record the
//! handler's proposed mutation for audit. The typed
//! [`AgentHookBus`] is the integration point for handlers that need
//! to mutate `LlmRequest` / tool call / tool result payloads in place
//! through the F08 typed views.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Convenience alias for the boxed futures returned by [`AgentHook`]
/// methods. The trait stays object-safe by erasing the concrete
/// future type behind `Pin<Box<dyn Future>>`, which mirrors the
/// pattern used elsewhere in the workspace (e.g. `LlmProvider`).
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Lifecycle points at which the agent fans out to registered handlers.
///
/// Every variant has a matching [`HookPayload`] case carrying typed
/// fields. Handlers that only care about a subset of events can use
/// `ctx.event` to filter cheaply before destructuring the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    /// Fired once per turn, immediately before the LLM request is sent.
    PreTurn,
    /// Fired immediately before a single tool call is executed.
    PreToolUse,
    /// Fired immediately after a single tool call returns.
    PostToolUse,
    /// Fired when a tool call returned a non-success status. Splits the
    /// failure path from [`HookEvent::PostToolUse`] so handlers can wire
    /// retry / SIEM-export logic without re-parsing the status field.
    PostToolUseFailure,
    /// Fired after a tool result is appended to the conversation.
    PostTool,
    /// Fired before a context compaction pass runs.
    PreCompact,
    /// Fired after a context compaction pass lands.
    PostCompact,
    /// Fired when a subagent is spawned.
    SubagentStart,
    /// Fired when a subagent terminates, so audit / replay handlers can
    /// capture the final transcript and exit reason.
    SubagentStop,
    /// Fired when a permission decision is about to be presented.
    PermissionRequest,
    /// Fired when a permission decision resolved as deny.
    PermissionDenied,
    /// Fired when the user submits a new prompt. Handlers may rewrite
    /// the prompt via the `prompt` field of `HookResult::mutate`.
    UserPromptSubmit,
    /// Fired at session start; companion to [`HookEvent::Setup`].
    SessionStart,
    /// Fired when the agent yields the turn back to the user without
    /// an outstanding tool call (clear-code's `Stop` semantics).
    Stop,
    /// Fired the first time the agent boots in a workspace, or when a
    /// maintenance task (config migration, index rebuild) completes.
    Setup,
}

/// Typed payload accompanying every [`HookEvent`].
///
/// Each variant carries the fields the agent loop guarantees for that
/// dispatch site. Handlers can either pattern-match on the variant
/// (`HookPayload::PreToolUse { tool_name, .. } => …`) or project to
/// JSON via [`HookContext::payload_json`] for legacy code paths and
/// for skill hook scripts that pipe the payload through stdin/env.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HookPayload {
    PreTurn {
        turn_id: String,
    },
    PreToolUse {
        turn_id: String,
        tool_name: String,
        call_id: String,
    },
    PostToolUse {
        turn_id: String,
        tool_name: String,
        call_id: String,
        status: String,
    },
    PostToolUseFailure {
        turn_id: String,
        tool_name: String,
        call_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    PostTool {
        turn_id: String,
        tool_name: String,
        call_id: String,
        status: String,
    },
    PreCompact {
        turn_id: String,
        before_tokens: u64,
    },
    PostCompact {
        turn_id: String,
        before_tokens: u64,
        after_tokens: u64,
    },
    SubagentStart {
        subagent_id: String,
        kind: String,
        parent_turn_id: String,
    },
    SubagentStop {
        subagent_id: String,
        kind: String,
        parent_turn_id: String,
        status: String,
    },
    PermissionRequest {
        capability: String,
        tool_name: String,
        turn_id: String,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    PermissionDenied {
        capability: String,
        tool_name: String,
        turn_id: String,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        reason: String,
    },
    UserPromptSubmit {
        prompt: String,
        turn_id: String,
    },
    SessionStart {
        session_id: String,
        reason: String,
    },
    Stop {
        turn_id: String,
    },
    Setup {
        workspace: String,
        reason: String,
    },
}

impl HookPayload {
    /// Return the [`HookEvent`] discriminant of this payload.
    pub fn event(&self) -> HookEvent {
        match self {
            HookPayload::PreTurn { .. } => HookEvent::PreTurn,
            HookPayload::PreToolUse { .. } => HookEvent::PreToolUse,
            HookPayload::PostToolUse { .. } => HookEvent::PostToolUse,
            HookPayload::PostToolUseFailure { .. } => HookEvent::PostToolUseFailure,
            HookPayload::PostTool { .. } => HookEvent::PostTool,
            HookPayload::PreCompact { .. } => HookEvent::PreCompact,
            HookPayload::PostCompact { .. } => HookEvent::PostCompact,
            HookPayload::SubagentStart { .. } => HookEvent::SubagentStart,
            HookPayload::SubagentStop { .. } => HookEvent::SubagentStop,
            HookPayload::PermissionRequest { .. } => HookEvent::PermissionRequest,
            HookPayload::PermissionDenied { .. } => HookEvent::PermissionDenied,
            HookPayload::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            HookPayload::SessionStart { .. } => HookEvent::SessionStart,
            HookPayload::Stop { .. } => HookEvent::Stop,
            HookPayload::Setup { .. } => HookEvent::Setup,
        }
    }
}

/// Per-event context passed to every [`HookHandler`].
///
/// `event` is the discriminant of [`HookContext::payload`] held as a
/// separate field so handlers can filter cheaply
/// (`if ctx.event != … { return … }`) without destructuring the enum.
/// The two stay in sync via [`HookContext::new`].
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub payload: HookPayload,
}

impl HookContext {
    pub fn new(payload: HookPayload) -> Self {
        let event = payload.event();
        Self { event, payload }
    }

    /// JSON projection of [`HookContext::payload`].
    ///
    /// Used by handlers that need a `serde_json::Value` (e.g. skill
    /// hooks that pipe the payload through `SQUEEZY_HOOK_PAYLOAD` to
    /// an external shell command). The projection is
    /// `serde_json::to_value` over the typed enum, so the JSON shape
    /// is stable per variant.
    pub fn payload_json(&self) -> Value {
        serde_json::to_value(&self.payload).unwrap_or(Value::Null)
    }
}

/// Result of one handler invocation.
///
/// `allow=false` advises the caller that the in-flight action should be
/// blocked; `mutate=Some(v)` carries a handler-proposed replacement for
/// the payload (e.g. a transformed turn instructions block). Today the
/// agent records these but does not yet apply mutations — that is left
/// to a follow-up commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub allow: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutate: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl HookResult {
    /// Convenience constructor for the common "no-op accept" reply.
    pub fn allow() -> Self {
        Self {
            allow: true,
            mutate: None,
            message: None,
        }
    }

    /// Convenience constructor for an outright deny.
    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            allow: false,
            mutate: None,
            message: Some(message.into()),
        }
    }
}

/// User-supplied logic that observes (and optionally mutates) an event.
///
/// Handlers run synchronously inside the agent loop; expensive work
/// belongs behind a channel or a background task started elsewhere.
/// The trait stays object-safe so the registry can erase handler types
/// behind `Box<dyn HookHandler>`.
pub trait HookHandler {
    fn handle(&self, ctx: &HookContext) -> HookResult;
}

/// Collection of handlers fanned out per dispatched event.
///
/// The registry is intentionally simple: handlers are stored in
/// insertion order and every handler sees every event. Filtering by
/// [`HookEvent`] is the handler's responsibility — the trait method
/// receives the event in `ctx.event`. This mirrors the codex
/// reference contract and keeps the registry oblivious to per-handler
/// subscription policy.
#[derive(Default)]
pub struct HookRegistry {
    handlers: Vec<Box<dyn HookHandler + Send + Sync>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new handler. Returns the registry by `&mut self` so
    /// callers can chain registrations.
    pub fn register(&mut self, handler: Box<dyn HookHandler + Send + Sync>) -> &mut Self {
        self.handlers.push(handler);
        self
    }

    /// Number of registered handlers. Primarily useful for tests.
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Whether the registry has no handlers. Callers can skip building
    /// a [`HookContext`] entirely when this is true.
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Fan out the typed payload to every handler and collect their
    /// replies. The event discriminant is derived from `payload` so
    /// dispatch sites never need to pass both.
    pub fn dispatch(&self, payload: HookPayload) -> Vec<HookResult> {
        let ctx = HookContext::new(payload);
        self.dispatch_context(&ctx)
    }

    /// Like [`HookRegistry::dispatch`] but accepts a pre-built context.
    pub fn dispatch_context(&self, ctx: &HookContext) -> Vec<HookResult> {
        self.handlers
            .iter()
            .map(|handler| handler.handle(ctx))
            .collect()
    }
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("handlers", &self.handlers.len())
            .finish()
    }
}

/// Mutable view of an outbound provider request handed to
/// [`AgentHook::before_provider_request`].
///
/// `payload` is intentionally kept as a `serde_json::Value` so this
/// crate does not have to depend on `squeezy-llm`; the agent rebuilds
/// the typed `LlmRequest` from the (possibly mutated) JSON payload
/// after the bus has run. Hooks should mutate `payload` in place to
/// rewrite the request before it is sent.
#[derive(Debug, Clone)]
pub struct LlmRequestView {
    /// Stable identifier for the turn issuing this request.
    pub turn_id: String,
    /// JSON-shaped request body. Hooks may rewrite this in place.
    pub payload: Value,
}

impl LlmRequestView {
    pub fn new(turn_id: impl Into<String>, payload: Value) -> Self {
        Self {
            turn_id: turn_id.into(),
            payload,
        }
    }
}

/// Mutable view of a tool call about to be executed. Handlers may
/// rewrite `arguments` in place to patch the call before
/// [`AgentHook::before_tool_call`] returns.
#[derive(Debug, Clone)]
pub struct ToolCallView {
    /// Identifier of the turn that issued the call.
    pub turn_id: String,
    /// Per-call identifier emitted by the provider.
    pub call_id: String,
    /// Registered tool name (e.g. `read_file`).
    pub tool_name: String,
    /// JSON-shaped argument object. Hooks may rewrite in place.
    pub arguments: Value,
}

impl ToolCallView {
    pub fn new(
        turn_id: impl Into<String>,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments,
        }
    }
}

/// Mutable view of a tool result before it is appended to the
/// conversation. Handlers may rewrite `output` (and `status`) in
/// place to redact, summarize, or annotate the result.
#[derive(Debug, Clone)]
pub struct ToolResultView {
    pub turn_id: String,
    pub call_id: String,
    pub tool_name: String,
    /// Outcome label (e.g. `"success"`, `"error"`, `"denied"`).
    pub status: String,
    /// JSON-shaped result payload. Hooks may rewrite in place.
    pub output: Value,
}

impl ToolResultView {
    pub fn new(
        turn_id: impl Into<String>,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        status: impl Into<String>,
        output: Value,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            status: status.into(),
            output,
        }
    }
}

/// Outcome of [`AgentHook::before_tool_call`].
///
/// Returning [`Decision::Deny`] short-circuits the bus and tells the
/// agent to skip the tool call, surfacing `message` to the model as
/// the would-be tool result. Later hooks in the bus are not invoked
/// after a deny so handler ordering is observable.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Continue executing the tool call.
    Allow,
    /// Skip the tool call; the agent surfaces `message` in place of
    /// the real result.
    Deny { message: String },
}

impl Decision {
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    #[must_use]
    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }
}

/// Typed mutation-capable extension surface for the agent loop.
///
/// Replaces the observation-only [`HookHandler`] pipeline as the
/// primary integration point for skills, MCP, telemetry, and any
/// future extension that needs to intercept the agent loop with
/// structured input. Each method takes a mutable view of the
/// relevant payload so handlers can rewrite the request, tool
/// arguments, or tool result in place. All methods have no-op
/// default implementations so concrete handlers only override the
/// lifecycle points they care about.
///
/// The trait is object-safe (no generic methods, no `Self: Sized`
/// bounds) so the dispatcher in [`AgentHookBus`] can store
/// heterogeneous handlers behind `Box<dyn AgentHook>`. Futures are
/// boxed via [`HookFuture`] to keep the trait dyn-compatible under
/// stable Rust.
pub trait AgentHook: Send + Sync {
    /// Fires before the agent issues an LLM request. Handlers may
    /// rewrite `req.payload` in place. The default implementation is
    /// a no-op.
    fn before_provider_request<'a>(&'a self, _req: &'a mut LlmRequestView) -> HookFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Fires before a tool call executes. Handlers may rewrite
    /// `call.arguments` in place; the returned [`Decision`] decides
    /// whether the call proceeds. The default implementation allows
    /// the call without mutation.
    fn before_tool_call<'a>(&'a self, _call: &'a mut ToolCallView) -> HookFuture<'a, Decision> {
        Box::pin(async { Decision::Allow })
    }

    /// Fires after a tool call completes, before the result is
    /// appended to the conversation. Handlers may rewrite the result
    /// payload in place. The default implementation is a no-op.
    fn after_tool_result<'a>(&'a self, _result: &'a mut ToolResultView) -> HookFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Fires before the agent swaps the active session for `target_id`
    /// (resume / quick-switch). Handlers can veto the swap by returning
    /// [`Decision::Deny`] — typical use is an "unsaved work" guard that
    /// blocks a switch while the user has a pending edit. The default
    /// implementation allows the switch. The bus short-circuits on the
    /// first deny, mirroring [`AgentHook::before_tool_call`].
    fn before_session_switch<'a>(&'a self, _target_id: &'a str) -> HookFuture<'a, Decision> {
        Box::pin(async { Decision::Allow })
    }
}

/// Sequential dispatcher for [`AgentHook`] implementations.
///
/// Owns a vector of trait objects and fans out lifecycle calls in
/// registration order. Each handler observes the mutations made by
/// the handlers that ran before it, so ordering is meaningful. The
/// bus is intentionally simple (no per-event filtering, no
/// priorities) so the agent loop can reach for it on the hot path
/// without ceremony.
#[derive(Default)]
pub struct AgentHookBus {
    hooks: Vec<Box<dyn AgentHook>>,
}

impl AgentHookBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new hook. Returns `&mut self` so callers can chain.
    pub fn register(&mut self, hook: Box<dyn AgentHook>) -> &mut Self {
        self.hooks.push(hook);
        self
    }

    /// Number of registered hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// `true` when no hooks are registered. Callers can skip the
    /// dispatch entirely on this path.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Fan out `before_provider_request` to every registered hook in
    /// registration order, awaiting each handler so later handlers
    /// see earlier mutations.
    pub async fn before_provider_request(&self, req: &mut LlmRequestView) {
        for hook in &self.hooks {
            hook.before_provider_request(req).await;
        }
    }

    /// Fan out `before_tool_call` and short-circuit on the first
    /// [`Decision::Deny`]. The (possibly mutated) `call` always
    /// reflects every handler that ran, including the one that
    /// denied.
    pub async fn before_tool_call(&self, call: &mut ToolCallView) -> Decision {
        for hook in &self.hooks {
            match hook.before_tool_call(call).await {
                Decision::Allow => continue,
                deny @ Decision::Deny { .. } => return deny,
            }
        }
        Decision::Allow
    }

    /// Fan out `after_tool_result` to every registered hook in
    /// registration order.
    pub async fn after_tool_result(&self, result: &mut ToolResultView) {
        for hook in &self.hooks {
            hook.after_tool_result(result).await;
        }
    }

    /// Fan out `before_session_switch` and short-circuit on the first
    /// [`Decision::Deny`]. The agent's session-switch path consults this
    /// before swapping the active session so handlers (e.g. an
    /// "unsaved work" guard) can veto the swap. Later hooks after the
    /// deny are not invoked, matching the
    /// [`AgentHookBus::before_tool_call`] contract.
    pub async fn before_session_switch(&self, target_id: &str) -> Decision {
        for hook in &self.hooks {
            match hook.before_session_switch(target_id).await {
                Decision::Allow => continue,
                deny @ Decision::Deny { .. } => return deny,
            }
        }
        Decision::Allow
    }
}

impl std::fmt::Debug for AgentHookBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHookBus")
            .field("hooks", &self.hooks.len())
            .finish()
    }
}

/// Adapter that bridges legacy observation-style [`HookHandler`]s
/// (registered against a [`HookRegistry`]) into the new typed
/// [`AgentHook`] surface.
///
/// Register one of these on the typed [`AgentHookBus`] to keep skill
/// hook scripts, telemetry sinks, and other existing handlers wired
/// while the rest of `squeezy-agent` migrates to the typed views.
///
/// Mutations proposed by legacy handlers via [`HookResult::mutate`]
/// remain advisory here — the typed views are not rewritten from the
/// legacy JSON reply — preserving the documented "mutations are
/// recorded but not yet applied" contract that squeezy-agent expects
/// today. Legacy deny replies for `PreToolUse` are honored by
/// translating them into a [`Decision::Deny`] so the typed dispatch
/// path inherits the same blocking behavior the observation path
/// already implements.
#[derive(Clone)]
pub struct LegacyHookForwarder {
    registry: Arc<HookRegistry>,
}

impl LegacyHookForwarder {
    #[must_use]
    pub fn new(registry: Arc<HookRegistry>) -> Self {
        Self { registry }
    }

    /// Borrow the wrapped registry. Useful for tests and for callers
    /// that want to introspect the legacy handler set.
    #[must_use]
    pub fn registry(&self) -> &Arc<HookRegistry> {
        &self.registry
    }
}

impl AgentHook for LegacyHookForwarder {
    fn before_provider_request<'a>(&'a self, req: &'a mut LlmRequestView) -> HookFuture<'a, ()> {
        Box::pin(async move {
            if self.registry.is_empty() {
                return;
            }
            let _ = self.registry.dispatch(HookPayload::PreTurn {
                turn_id: req.turn_id.clone(),
            });
        })
    }

    fn before_tool_call<'a>(&'a self, call: &'a mut ToolCallView) -> HookFuture<'a, Decision> {
        Box::pin(async move {
            if self.registry.is_empty() {
                return Decision::Allow;
            }
            let results = self.registry.dispatch(HookPayload::PreToolUse {
                turn_id: call.turn_id.clone(),
                tool_name: call.tool_name.clone(),
                call_id: call.call_id.clone(),
            });
            for result in results {
                if !result.allow {
                    let message = result
                        .message
                        .unwrap_or_else(|| "tool call denied by legacy hook".to_string());
                    return Decision::Deny { message };
                }
            }
            Decision::Allow
        })
    }

    fn after_tool_result<'a>(&'a self, result: &'a mut ToolResultView) -> HookFuture<'a, ()> {
        Box::pin(async move {
            if self.registry.is_empty() {
                return;
            }
            let _ = self.registry.dispatch(HookPayload::PostToolUse {
                turn_id: result.turn_id.clone(),
                tool_name: result.tool_name.clone(),
                call_id: result.call_id.clone(),
                status: result.status.clone(),
            });
        })
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
