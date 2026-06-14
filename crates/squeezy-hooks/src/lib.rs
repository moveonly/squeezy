//! Minimal hook system for Squeezy.
//!
//! [`HookHandler`] is the synchronous observation-style API keyed by
//! [`HookEvent`]. Skill hook scripts and the squeezy-agent dispatch
//! sites consume it via the [`HookRegistry`].
//!
//! The registry is plugged into `squeezy-agent` and fans events out
//! for every variant of [`HookEvent`] from the natural site listed on
//! each variant's doc comment.
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
//! handler's proposed mutation for audit.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

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
    /// an outstanding tool call.
    Stop,
    /// Fired the first time the agent boots in a workspace, or when a
    /// maintenance task (config migration, index rebuild) completes.
    Setup,
}

impl HookEvent {
    /// Returns `true` for events where a handler deny result is actually
    /// enforced by the agent loop. Observation-only events accept the
    /// deny field in [`HookResult`] but the caller ignores it.
    ///
    /// This lets documentation, parsers, and diagnostic tools clearly
    /// separate enforcement-capable events from observation-only events.
    pub fn is_enforcement_capable(self) -> bool {
        matches!(self, HookEvent::PreToolUse | HookEvent::PermissionRequest)
    }
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
pub struct HookContext {
    pub event: HookEvent,
    pub payload: HookPayload,
    /// Lazily-computed JSON projection of `payload`. Populated on
    /// first call to [`HookContext::payload_json`] so multiple
    /// handlers on the same dispatch share one serialization.
    json_cache: OnceLock<Value>,
}

impl HookContext {
    pub fn new(payload: HookPayload) -> Self {
        let event = payload.event();
        Self {
            event,
            payload,
            json_cache: OnceLock::new(),
        }
    }

    /// JSON projection of [`HookContext::payload`].
    ///
    /// Used by handlers that need a `serde_json::Value` (e.g. skill
    /// hooks that pipe the payload through `SQUEEZY_HOOK_PAYLOAD` to
    /// an external shell command). The projection is computed once per
    /// dispatch and cached for subsequent calls, avoiding redundant
    /// serialization when multiple handlers fire on the same event.
    pub fn payload_json(&self) -> Value {
        self.json_cache
            .get_or_init(|| serde_json::to_value(&self.payload).unwrap_or(Value::Null))
            .clone()
    }
}

impl std::fmt::Debug for HookContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookContext")
            .field("event", &self.event)
            .field("payload", &self.payload)
            .finish()
    }
}

impl Clone for HookContext {
    fn clone(&self) -> Self {
        Self {
            event: self.event,
            payload: self.payload.clone(),
            json_cache: OnceLock::new(),
        }
    }
}

/// Result of one handler invocation.
///
/// `allow=false` advises the caller that the in-flight action should be
/// blocked; `mutate=Some(v)` carries a handler-proposed replacement for
/// the payload (e.g. a transformed turn instructions block or a rewritten
/// user prompt). Mutations from in-process handlers are applied at the
/// [`HookEvent::PreTurn`] and [`HookEvent::UserPromptSubmit`] dispatch
/// sites. Skill hook scripts cannot return mutations because their stdout
/// is ignored.
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
/// Handlers registered with [`HookRegistry::register`] observe every
/// event (the handler filters by `ctx.event`). Handlers registered
/// with [`HookRegistry::register_for_event`] are indexed by event and
/// only invoked for that specific event, giving O(matching handlers)
/// dispatch instead of O(total handlers).
#[derive(Default)]
pub struct HookRegistry {
    /// Handlers that observe all events. Added via [`HookRegistry::register`].
    all_handlers: Vec<Box<dyn HookHandler + Send + Sync>>,
    /// Handlers indexed by the specific event they subscribe to.
    /// Added via [`HookRegistry::register_for_event`].
    event_handlers: BTreeMap<HookEvent, Vec<Box<dyn HookHandler + Send + Sync>>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler that will be invoked for every event.
    /// Returns `&mut self` for chaining.
    pub fn register(&mut self, handler: Box<dyn HookHandler + Send + Sync>) -> &mut Self {
        self.all_handlers.push(handler);
        self
    }

    /// Register a handler that will only be invoked when `event`
    /// matches the dispatched payload. Prefer this over [`HookRegistry::register`]
    /// when the handler is only relevant for a single event — it avoids
    /// calling every handler on unrelated dispatches.
    pub fn register_for_event(
        &mut self,
        event: HookEvent,
        handler: Box<dyn HookHandler + Send + Sync>,
    ) -> &mut Self {
        self.event_handlers.entry(event).or_default().push(handler);
        self
    }

    /// Total number of registered handlers across all buckets.
    pub fn len(&self) -> usize {
        self.all_handlers.len() + self.event_handlers.values().map(|v| v.len()).sum::<usize>()
    }

    /// Whether the registry has no handlers. Callers can skip building
    /// a [`HookContext`] entirely when this is true.
    pub fn is_empty(&self) -> bool {
        self.all_handlers.is_empty() && self.event_handlers.is_empty()
    }

    /// Fan out the typed payload to every handler and collect their
    /// replies. The event discriminant is derived from `payload` so
    /// dispatch sites never need to pass both.
    pub fn dispatch(&self, payload: HookPayload) -> Vec<HookResult> {
        if self.is_empty() {
            return Vec::new();
        }
        let ctx = HookContext::new(payload);
        self.dispatch_context(&ctx)
    }

    /// Like [`HookRegistry::dispatch`] but accepts a pre-built context.
    pub fn dispatch_context(&self, ctx: &HookContext) -> Vec<HookResult> {
        let event_count = self.event_handlers.get(&ctx.event).map_or(0, |v| v.len());
        let mut results = Vec::with_capacity(self.all_handlers.len() + event_count);
        for handler in &self.all_handlers {
            results.push(handler.handle(ctx));
        }
        if let Some(handlers) = self.event_handlers.get(&ctx.event) {
            for handler in handlers {
                results.push(handler.handle(ctx));
            }
        }
        results
    }

    /// Fan out `payload` to every handler without retaining replies.
    ///
    /// Use this for observation-only sites that only need handler side
    /// effects; decision points should continue using [`HookRegistry::dispatch`].
    pub fn dispatch_no_collect(&self, payload: HookPayload) {
        if self.is_empty() {
            return;
        }
        let ctx = HookContext::new(payload);
        self.dispatch_context_no_collect(&ctx);
    }

    /// Like [`HookRegistry::dispatch_no_collect`] but accepts a pre-built
    /// context.
    pub fn dispatch_context_no_collect(&self, ctx: &HookContext) {
        for handler in &self.all_handlers {
            let _ = handler.handle(ctx);
        }
        if let Some(handlers) = self.event_handlers.get(&ctx.event) {
            for handler in handlers {
                let _ = handler.handle(ctx);
            }
        }
    }
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let event_count: usize = self.event_handlers.values().map(|v| v.len()).sum();
        f.debug_struct("HookRegistry")
            .field("all_handlers", &self.all_handlers.len())
            .field("event_handlers", &event_count)
            .finish()
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
