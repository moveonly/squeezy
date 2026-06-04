use super::*;
use squeezy_core::{
    DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS, PermissionCapability, PermissionRequest,
    PermissionRisk,
};

#[test]
fn auto_allow_ceiling_is_capability_aware() {
    use PermissionCapability::{Edit, Mcp, Network, Shell};
    // Workspace-mutation capabilities may auto-approve up to High (blast radius
    // is confined to the workspace; outside writes are escalated separately).
    assert!(within_auto_allow_ceiling(Edit, PermissionRisk::High));
    assert!(within_auto_allow_ceiling(Shell, PermissionRisk::High));
    // Reach-out capabilities cap at Medium — a High-risk network/MCP call (e.g.
    // `curl … -d @secret`) must reach a human.
    assert!(within_auto_allow_ceiling(Network, PermissionRisk::Medium));
    assert!(!within_auto_allow_ceiling(Network, PermissionRisk::High));
    assert!(!within_auto_allow_ceiling(Mcp, PermissionRisk::High));
    // Critical is never auto-approved, regardless of capability.
    assert!(!within_auto_allow_ceiling(Edit, PermissionRisk::Critical));
    assert!(!within_auto_allow_ceiling(Shell, PermissionRisk::Critical));
}

#[test]
fn reviewer_may_auto_allow_gates_on_allowlist_workspace_and_ceiling() {
    use PermissionCapability::{Destructive, Edit, Shell};
    let allow = vec![Edit, Shell];

    let mut edit = sample_request("write_file", "path:src/a.rs");
    edit.capability = Edit;
    edit.risk = PermissionRisk::High;
    assert!(reviewer_may_auto_allow(&allow, &edit));

    // Not allowlisted -> withheld.
    let mut destructive = edit.clone();
    destructive.capability = Destructive;
    assert!(!reviewer_may_auto_allow(&allow, &destructive));

    // Outside the workspace -> withheld even when allowlisted & within ceiling.
    let mut outside = edit.clone();
    outside
        .metadata
        .insert("outside_workspace".to_string(), "true".to_string());
    assert!(!reviewer_may_auto_allow(&allow, &outside));

    // Over the ceiling -> withheld.
    let mut critical = edit.clone();
    critical.risk = PermissionRisk::Critical;
    assert!(!reviewer_may_auto_allow(&allow, &critical));
}

#[test]
fn bounded_transcript_keeps_last_user_with_caps() {
    let mut items = (0..20)
        .map(|index| TranscriptItem::assistant(format!("assistant {index} {}", "a".repeat(2400))))
        .collect::<Vec<_>>();
    items.push(TranscriptItem::user(
        "important final user request".to_string(),
    ));
    let snapshot = AiReviewerTranscriptSnapshot {
        entry_count: items.len(),
        history_version: 0,
        items,
    };
    let rendered = bounded_transcript(&snapshot, None, DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS);
    assert!(rendered.contains("important final user request"));
    assert!(approx_tokens(&rendered) <= DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS + 200);
    assert!(!rendered.contains("assistant 0"));
}

#[test]
fn bounded_transcript_compacts_older_into_summary() {
    let mut items: Vec<TranscriptItem> = Vec::new();
    items.push(TranscriptItem::user(
        "original intent: refactor permissions broker".to_string(),
    ));
    for index in 0..30 {
        items.push(TranscriptItem::assistant(format!(
            "assistant turn {index} did some intermediate work"
        )));
    }
    items.push(TranscriptItem::user("latest follow-up".to_string()));
    let snapshot = AiReviewerTranscriptSnapshot {
        entry_count: items.len(),
        history_version: 0,
        items,
    };
    let rendered = bounded_transcript(&snapshot, None, DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS);
    assert!(rendered.contains("summary of"));
    assert!(rendered.contains("earlier turn(s)"));
    assert!(rendered.contains("latest follow-up"));
    // The earliest assistant turn should be elided into the summary line.
    assert!(!rendered.contains("assistant turn 0 "));
}

#[test]
fn bounded_transcript_respects_small_budget() {
    let items = (0..40)
        .map(|index| TranscriptItem::user(format!("user message {index}")))
        .collect::<Vec<_>>();
    let snapshot = AiReviewerTranscriptSnapshot {
        entry_count: items.len(),
        history_version: 0,
        items,
    };
    let rendered = bounded_transcript(&snapshot, None, 512);
    // The most recent user message must always be present.
    assert!(rendered.contains("user message 39"));
    // The very first message should be folded into the summary, not printed.
    assert!(!rendered.contains("39:user: user message 0"));
}

#[test]
fn reviewer_policy_extra_appends_to_base() {
    let mut config = squeezy_core::AppConfig::from_env();
    // No inline policy -> just the bundled base policy.
    config.permissions.ai_reviewer.policy = None;
    let base = load_policy(&config).expect("base policy");
    assert!(!base.contains("Additional project policy"));

    config.permissions.ai_reviewer.policy = Some("Never touch vendored files.".to_string());
    let combined = load_policy(&config).expect("combined policy");
    assert!(
        combined.starts_with(&base),
        "inline policy must be appended to the base, not replace it"
    );
    assert!(combined.contains("Additional project policy"));
    assert!(combined.contains("Never touch vendored files."));
}

#[test]
fn parse_reviewer_json_inside_text() {
    let decision =
        parse_reviewer_response("```json\n{\"action\":\"deny\",\"reason\":\"too broad\"}\n```")
            .expect("decision");
    assert_eq!(decision.action, PermissionAction::Deny);
    assert_eq!(decision.reason, "too broad");
}

#[test]
fn circuit_trips_after_consecutive_denials() {
    let mut state = AiReviewerState::default();
    assert!(state.record_denial(TurnId::new(7)).is_none());
    let reason = state.record_denial(TurnId::new(7)).expect("tripped");
    assert!(reason.contains("consecutively"));
    assert!(state.bypass_reason(TurnId::new(7)).is_some());
}

#[test]
fn transcript_delta_marker_mentions_prior_entries() {
    let mut state = AiReviewerState::default();
    let first = AiReviewerTranscriptSnapshot {
        items: vec![TranscriptItem::user("one")],
        history_version: 2,
        entry_count: 1,
    };
    let second = AiReviewerTranscriptSnapshot {
        items: vec![
            TranscriptItem::user("one"),
            TranscriptItem::assistant("two"),
        ],
        history_version: 2,
        entry_count: 2,
    };
    assert!(state.transcript_delta_marker(&first).is_none());
    assert_eq!(
        state.transcript_delta_marker(&second),
        Some("[1 earlier entries reviewed previously and unchanged]".to_string())
    );
}

#[test]
fn prompt_contains_policy_and_request() {
    let config = AppConfig::default();
    let mut state = AiReviewerState::default();
    let request = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "read_file".to_string(),
        capability: PermissionCapability::Read,
        target: "path:README.md".to_string(),
        risk: PermissionRisk::Low,
        summary: "read README".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };
    let prompt = build_review_prompt(&config, &request, None, "test policy", &mut state);
    assert!(prompt.contains("test policy"));
    assert!(prompt.contains("\"capability\":\"read\""));
    assert!(prompt.contains("\"target\":\"path:README.md\""));
}

fn sample_request(tool_name: &str, target: &str) -> PermissionRequest {
    PermissionRequest {
        call_id: "call".to_string(),
        tool_name: tool_name.to_string(),
        capability: PermissionCapability::Shell,
        target: target.to_string(),
        risk: PermissionRisk::Medium,
        summary: "sample".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    }
}

#[test]
fn record_audit_captures_reason_and_caps_at_ring_size() {
    let mut state = AiReviewerState::default();
    let turn = TurnId::new(1);
    state.record_audit(
        turn,
        &sample_request("shell.run", "command:ls"),
        ReviewerAuditVerdict::Allow,
        "approved low-risk listing",
    );
    state.record_audit(
        turn,
        &sample_request("shell.run", "command:rm -rf /"),
        ReviewerAuditVerdict::Deny,
        "destructive root operation",
    );
    let entries = state.recent_decisions();
    assert_eq!(entries.len(), 2);
    let first = &entries[0];
    assert_eq!(first.verdict, ReviewerAuditVerdict::Allow);
    assert_eq!(first.tool_name, "shell.run");
    assert_eq!(first.target, "command:ls");
    assert_eq!(first.reason, "approved low-risk listing");
    let second = &entries[1];
    assert_eq!(second.verdict, ReviewerAuditVerdict::Deny);
    assert_eq!(second.reason, "destructive root operation");

    // Overflow the ring; oldest entry is evicted while capacity stays at 50.
    for i in 0..AUDIT_RING_CAPACITY {
        state.record_audit(
            turn,
            &sample_request("shell.run", &format!("command:echo {i}")),
            ReviewerAuditVerdict::NoDecision,
            "filler",
        );
    }
    let entries = state.recent_decisions();
    assert_eq!(entries.len(), AUDIT_RING_CAPACITY);
    // The two original entries should now be evicted.
    assert!(
        entries
            .iter()
            .all(|entry| entry.verdict == ReviewerAuditVerdict::NoDecision)
    );
}

/// The reviewer schema (M13) must mirror exactly what
/// `parse_reviewer_response` deserializes into `ReviewerDecision`: the
/// `action` enum carries the three canonical `PermissionMode` strings and
/// `reason` is a string. Any document that validates against the schema
/// must parse back into the same action — the schema cannot drift from the
/// parse target without this failing.
#[test]
fn reviewer_output_schema_mirrors_reviewer_decision() {
    let schema = reviewer_output_schema();
    assert!(schema.strict, "reviewer schema must be strict");

    let props = &schema.schema["properties"];
    let action_enum = props["action"]["enum"]
        .as_array()
        .expect("action carries an enum");
    let values: Vec<&str> = action_enum.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        values,
        vec![
            PermissionAction::Allow.as_str(),
            PermissionAction::Ask.as_str(),
            PermissionAction::Deny.as_str(),
        ],
        "action enum is the canonical PermissionMode set"
    );
    assert_eq!(props["reason"]["type"], "string");
    assert_eq!(
        schema.schema["required"],
        serde_json::json!(["action", "reason"])
    );
    assert_eq!(
        schema.schema["additionalProperties"],
        serde_json::json!(false)
    );

    for action in [
        PermissionAction::Allow,
        PermissionAction::Ask,
        PermissionAction::Deny,
    ] {
        let doc = serde_json::json!({ "action": action.as_str(), "reason": "ok" }).to_string();
        let decision = parse_reviewer_response(&doc).expect("schema doc parses");
        assert_eq!(decision.action, action);
        assert_eq!(decision.reason, "ok");
    }
}
