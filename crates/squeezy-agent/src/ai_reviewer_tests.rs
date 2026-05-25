use super::*;
use squeezy_core::{PermissionCapability, PermissionRequest, PermissionRisk};

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
    let rendered = bounded_transcript(&snapshot, None);
    assert!(rendered.contains("important final user request"));
    assert!(approx_tokens(&rendered) <= MAX_TRANSCRIPT_TOKENS + 20);
    assert!(!rendered.contains("assistant 0"));
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
