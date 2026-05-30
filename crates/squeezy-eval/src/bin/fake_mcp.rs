//! Minimal MCP stdio server used as an eval fixture.
//!
//! Speaks line-delimited JSON-RPC 2.0 on stdin/stdout per the
//! Model Context Protocol spec. Exposes three tools that exercise the
//! happy-path and a couple of edge cases without needing network or
//! external runtimes:
//!
//! - `echo` — returns the `message` argument as a text content block.
//! - `add`  — returns `a + b` as a text content block.
//! - `fail` — always returns `isError: true` so the agent's error
//!   surfacing path is exercised.
//!
//! The server understands `initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`, and `ping`; everything else is answered
//! with a JSON-RPC method-not-found error so the rmcp client doesn't
//! hang waiting on a reply.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                let _ = writeln!(
                    io::stderr(),
                    "fake-mcp: ignoring non-JSON line: {err}: {line}"
                );
                continue;
            }
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();
        if method.starts_with("notifications/") {
            // Notifications carry no `id` and never expect a reply.
            continue;
        }
        let reply = match method {
            "initialize" => respond(
                id,
                json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "squeezy-fake-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            ),
            "ping" => respond(id, json!({})),
            "tools/list" => respond(id, json!({ "tools": tool_descriptors() })),
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                handle_tool_call(id, &params)
            }
            "" => continue,
            other => error_response(id, -32601, format!("method not found: {other}")),
        };
        if let Ok(bytes) = serde_json::to_vec(&reply) {
            if out.write_all(&bytes).is_err() {
                break;
            }
            if out.write_all(b"\n").is_err() {
                break;
            }
            let _ = out.flush();
        }
    }
}

fn tool_descriptors() -> Value {
    json!([
        {
            "name": "echo",
            "description": "Echo the supplied message back as text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            }
        },
        {
            "name": "add",
            "description": "Return the sum of two integers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "b": { "type": "integer" }
                },
                "required": ["a", "b"]
            }
        },
        {
            "name": "fail",
            "description": "Always returns isError: true with a fixed message.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn handle_tool_call(id: Option<Value>, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "echo" => {
            let message = args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            respond(
                id,
                json!({
                    "content": [{ "type": "text", "text": message }],
                    "isError": false,
                }),
            )
        }
        "add" => {
            let a = args.get("a").and_then(Value::as_i64);
            let b = args.get("b").and_then(Value::as_i64);
            match (a, b) {
                (Some(a), Some(b)) => respond(
                    id,
                    json!({
                        "content": [
                            { "type": "text", "text": format!("{}", a + b) }
                        ],
                        "isError": false,
                    }),
                ),
                _ => respond(
                    id,
                    json!({
                        "content": [
                            { "type": "text", "text": "add requires integer `a` and `b`" }
                        ],
                        "isError": true,
                    }),
                ),
            }
        }
        "fail" => respond(
            id,
            json!({
                "content": [{ "type": "text", "text": "fixture failure" }],
                "isError": true,
            }),
        ),
        other => error_response(id, -32602, format!("unknown tool: {other}")),
    }
}

fn respond(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

fn error_response(id: Option<Value>, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": { "code": code, "message": message },
    })
}
