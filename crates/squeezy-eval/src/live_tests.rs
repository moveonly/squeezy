use super::*;
use std::sync::Arc;

/// Shared in-memory writer the tests can read back from.
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture(f: impl FnOnce(&LivePrinter)) -> String {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let printer = LivePrinter::new(Box::new(SharedBuf(buf.clone())), true);
    f(&printer);
    printer.flush();
    String::from_utf8_lossy(&buf.lock().unwrap()).into_owned()
}

#[test]
fn streams_assistant_delta_into_chunk() {
    let out = capture(|p| {
        p.event(
            &EvalEventKind::AssistantDelta {
                delta: "Hello ".into(),
            },
            Some("T(1)"),
        );
        p.event(
            &EvalEventKind::AssistantDelta {
                delta: "world.".into(),
            },
            Some("T(1)"),
        );
    });
    // The chunk opens with a 💬 marker, indents continuation, and contains the full text.
    assert!(out.contains("💬"));
    assert!(out.contains("Hello world."));
}

#[test]
fn announces_tool_call_started_and_completed() {
    let out = capture(|p| {
        p.event(
            &EvalEventKind::ToolCallStarted {
                call: serde_json::json!({"name": "grep", "arguments": {"pattern": "X"}}),
                origin: "model".to_string(),
            },
            Some("T(1)"),
        );
        p.event(
            &EvalEventKind::ToolCallCompleted {
                result: serde_json::json!({
                    "tool_name": "grep",
                    "status": "Success",
                    "cost_hint": {"output_bytes": 1234},
                }),
            },
            Some("T(1)"),
        );
    });
    assert!(out.contains("🔧 grepping for `X`"));
    assert!(out.contains("✅ grep (1234B)"));
}

#[test]
fn flags_findings_and_failures() {
    let out = capture(|p| {
        p.event(
            &EvalEventKind::Finding {
                rule_id: "duplicate_tool_call".into(),
                severity: "major".into(),
                summary: "grep fired 3 times".into(),
            },
            Some("T(1)"),
        );
        p.event(
            &EvalEventKind::TurnFailed {
                error: "boom".into(),
            },
            Some("T(2)"),
        );
    });
    assert!(out.contains("🔎 finding [major] duplicate_tool_call"));
    assert!(out.contains("🚨 turn failed: boom"));
}

#[test]
fn respects_enabled_flag() {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let printer = LivePrinter::new(Box::new(SharedBuf(buf.clone())), false);
    printer.event(
        &EvalEventKind::AssistantDelta {
            delta: "should not appear".into(),
        },
        Some("T(1)"),
    );
    assert!(buf.lock().unwrap().is_empty());
}
