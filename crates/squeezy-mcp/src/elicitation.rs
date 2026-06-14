use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use rmcp::{
    model::CreateElicitationRequestParams,
    service::{RequestContext, RoleClient},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use squeezy_core::PermissionMode;

pub(crate) const MCP_AUDIT_LOG_CAPACITY: usize = 256;

pub type McpElicitationFuture = Pin<Box<dyn Future<Output = McpElicitationResponse> + Send>>;
pub type McpElicitationHandler =
    Arc<dyn Fn(McpElicitationRequest) -> McpElicitationFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpElicitationKind {
    Form,
    Url,
}

impl McpElicitationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Url => "url",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationRequest {
    pub server: String,
    pub request_id: String,
    pub kind: McpElicitationKind,
    pub message: String,
    pub schema: Option<Value>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpElicitationAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationResponse {
    pub action: McpElicitationAction,
    pub content: Option<Value>,
}

impl McpElicitationResponse {
    pub fn accept(content: Option<Value>) -> Self {
        Self {
            action: McpElicitationAction::Accept,
            content,
        }
    }

    pub fn decline() -> Self {
        Self {
            action: McpElicitationAction::Decline,
            content: None,
        }
    }

    pub fn cancel() -> Self {
        Self {
            action: McpElicitationAction::Cancel,
            content: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpElicitationAuditOutcome {
    AutoAccepted,
    AutoDeclined,
    Forwarded,
}

impl McpElicitationAuditOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoAccepted => "auto_accepted",
            Self::AutoDeclined => "auto_declined",
            Self::Forwarded => "forwarded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationAuditEvent {
    pub server: String,
    pub request_id: String,
    pub kind: McpElicitationKind,
    pub policy: PermissionMode,
    pub outcome: McpElicitationAuditOutcome,
    pub unix_millis: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoElicitationDecision {
    AutoAccept,
    AutoDecline,
    Forward,
}

pub(crate) fn elicitation_kind(request: &CreateElicitationRequestParams) -> McpElicitationKind {
    match request {
        CreateElicitationRequestParams::FormElicitationParams { .. } => McpElicitationKind::Form,
        CreateElicitationRequestParams::UrlElicitationParams { .. } => McpElicitationKind::Url,
    }
}

pub(crate) fn classify_elicitation(
    policy: PermissionMode,
    request: &CreateElicitationRequestParams,
) -> AutoElicitationDecision {
    match policy {
        PermissionMode::Deny => AutoElicitationDecision::AutoDecline,
        PermissionMode::Allow if can_auto_accept_elicitation(request) => {
            AutoElicitationDecision::AutoAccept
        }
        PermissionMode::Allow | PermissionMode::Ask => AutoElicitationDecision::Forward,
    }
}

pub(crate) fn push_elicitation_audit(
    log: &Arc<Mutex<std::collections::VecDeque<McpElicitationAuditEvent>>>,
    event: McpElicitationAuditEvent,
) {
    if let Ok(mut log) = log.lock() {
        if log.len() >= MCP_AUDIT_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(event);
    }
}

pub(crate) fn elicitation_request_for_ui(
    server: &str,
    context: &RequestContext<RoleClient>,
    request: &CreateElicitationRequestParams,
) -> McpElicitationRequest {
    match request {
        CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => McpElicitationRequest {
            server: server.to_string(),
            request_id: format!("{:?}", context.id),
            kind: McpElicitationKind::Form,
            message: message.clone(),
            schema: serde_json::to_value(requested_schema).ok(),
            url: None,
            elicitation_id: None,
        },
        CreateElicitationRequestParams::UrlElicitationParams {
            message,
            url,
            elicitation_id,
            ..
        } => McpElicitationRequest {
            server: server.to_string(),
            request_id: format!("{:?}", context.id),
            kind: McpElicitationKind::Url,
            message: message.clone(),
            schema: None,
            url: Some(url.clone()),
            elicitation_id: Some(elicitation_id.clone()),
        },
    }
}

fn can_auto_accept_elicitation(request: &CreateElicitationRequestParams) -> bool {
    match request {
        CreateElicitationRequestParams::FormElicitationParams {
            requested_schema, ..
        } => requested_schema
            .required
            .as_ref()
            .map(|required| required.is_empty())
            .unwrap_or(true),
        CreateElicitationRequestParams::UrlElicitationParams { .. } => false,
    }
}
