//! Minimal hook system for Squeezy.
//!
//! Skills and other agent extensions can register handlers against
//! lifecycle events ([`HookEvent`]). The agent loop dispatches events
//! through a [`HookRegistry`]; each registered [`HookHandler`] returns a
//! [`HookResult`] that can advise the caller to deny the action or
//! mutate its payload.
//!
//! The dispatched call sites today are `PreTurn`, `PreToolUse`,
//! `PostToolUse`, `PreCompact`, and `PostCompact` (see
//! `squeezy-agent`). The remaining variants — `PostToolUseFailure`,
//! `PostTool`, `SubagentStart`, `SubagentStop`, `PermissionRequest`,
//! `PermissionDenied`, `UserPromptSubmit`, `SessionStart`, `Stop`,
//! `Setup` — are reserved as named enum entries so handlers can match
//! against them now and follow-up call-site wiring can land
//! incrementally without forcing the trait surface to evolve.
//!
//! Mutation results are recorded by the agent today but not yet
//! applied; the contract here is shaped for a future commit that wires
//! mutation into the per-turn instruction pipeline.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle points at which the agent fans out to registered handlers.
///
/// The variant set spans tool execution (`PreToolUse`, `PostToolUse`,
/// `PostToolUseFailure`, `PostTool`), permission gating
/// (`PermissionRequest`, `PermissionDenied`), subagent boundaries
/// (`SubagentStart`, `SubagentStop`), compaction (`PreCompact`,
/// `PostCompact`), and session boundaries (`PreTurn`,
/// `UserPromptSubmit`, `SessionStart`, `Stop`, `Setup`).
/// [`HookEvent::PreTurn`], [`HookEvent::PreToolUse`],
/// [`HookEvent::PostToolUse`], [`HookEvent::PreCompact`], and
/// [`HookEvent::PostCompact`] are currently dispatched; the remaining
/// variants are reserved so handler implementations can statically
/// declare interest in them before the agent wires the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    /// Fired once per turn, immediately before the LLM request is sent.
    PreTurn,
    /// Fired immediately before a single tool call is executed. Payload
    /// carries `{ "tool_name", "call_id", "turn_id" }` so handlers can
    /// inspect (today) and later rewrite (deferred) tool input.
    PreToolUse,
    /// Fired immediately after a single tool call returns. Payload
    /// mirrors `PreToolUse` and adds `{ "status" }` so handlers can
    /// audit outcomes.
    PostToolUse,
    /// Fired when a tool call returned a non-success status. Splits the
    /// failure path from [`HookEvent::PostToolUse`] so handlers can wire
    /// retry / SIEM-export logic without re-parsing the status field.
    PostToolUseFailure,
    /// Fired after a tool result is appended to the conversation.
    PostTool,
    /// Fired before a context compaction pass runs.
    PreCompact,
    /// Fired after a context compaction pass lands, with the before/after
    /// token counts in the payload so observers can react to the rewrite.
    PostCompact,
    /// Fired when a subagent is spawned.
    SubagentStart,
    /// Fired when a subagent terminates, so audit / replay handlers can
    /// capture the final transcript and exit reason.
    SubagentStop,
    /// Fired when a permission decision is about to be presented.
    PermissionRequest,
    /// Fired when a permission decision resolved as deny. Lets handlers
    /// nudge the model with a retry hint or escalate the denial to an
    /// out-of-band audit channel.
    PermissionDenied,
    /// Fired when the user submits a new prompt. Lets handlers append
    /// `additionalContexts` (e.g. current git branch, on-call rotation)
    /// before the turn begins.
    UserPromptSubmit,
    /// Fired at session start. Companion to [`HookEvent::Setup`]; this
    /// variant signals "agent is live for this run", while `Setup`
    /// signals "agent installation completed (first launch or
    /// maintenance)".
    SessionStart,
    /// Fired when the agent yields the turn back to the user without an
    /// outstanding tool call (clear-code's `Stop` semantics).
    Stop,
    /// Fired the first time the agent boots in a workspace, or when a
    /// maintenance task (config migration, index rebuild) completes.
    Setup,
}

/// Per-event payload passed to every [`HookHandler`].
///
/// `payload` is intentionally untyped at the registry level: each event
/// has its own JSON shape (e.g. `PreTurn` carries `{ "turn_index": N }`)
/// and individual handlers parse the fields they care about. This keeps
/// the registry independent of agent-internal types.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub payload: Value,
}

impl HookContext {
    pub fn new(event: HookEvent, payload: Value) -> Self {
        Self { event, payload }
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

    /// Fan out the event to every handler and collect their replies.
    ///
    /// `event` is folded into the constructed [`HookContext`] for
    /// convenience; callers may also pre-build the context and reach
    /// for [`HookRegistry::dispatch_context`] directly.
    pub fn dispatch(&self, event: HookEvent, payload: Value) -> Vec<HookResult> {
        let ctx = HookContext::new(event, payload);
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
