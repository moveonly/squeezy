# Structured Tool-Output Extraction

## Motivation

Tool output is the dominant input channel for an agentic coding loop. The
model writes one tool call and reads back whatever the tool produces:
build logs, test reports, grep hits, file bodies. Left unshaped, every
one of those streams pushes the prompt budget toward its ceiling in ways
that have nothing to do with the *answer* the agent needs.

A bare `cargo build` on a medium-sized workspace dumps 50–500 KB of
progress chatter (`Compiling foo v1.0`, `Downloading crates ...`,
`Fresh bar`, twin-emitted diagnostics for `(lib)` and `(lib test)`
targets) around the small number of `error` / `warning` JSON records that
actually drive the next decision. A single `cargo test` adds libtest's
plain-text harness output mixed into the JSON stream. A `grep` for a
common token returns thousands of identical-looking lines. A `read_file`
on a 4 MB asset attempts to UTF-8-decode JPEG bytes and corrupts the
context window. A `git diff` is structured data whose blank lines are
load-bearing and whose `+`/`-` markers must survive byte-for-byte.

Squeezy's shaping layer treats each of these as a separate extraction
problem. Cargo and rustc are parsed as JSON event streams. Nextest is
aggregated into pass/fail counters. Pytest and Jest are flattened into
signal lines plus a summary line. Unstructured shell output is
deduplicated, head/tail-windowed, and noise-filtered. Shaped blocks are
trimmed; the original bytes are spilled to a session-scoped tempfile so
the model can recover them on demand. Grep is byte-capped and per-line
truncated. `read_file` honours `diff_only` and switches to base64 +
MIME when it detects image magic bytes. Every reduction names the
escape hatch (`read_tool_output`) so the model can pivot to the raw
bytes when shaping discards something it needs.

The result is that the model sees the *signal* — the diagnostic, the
failing test name, the matching line — instead of the *stream*.

## Mechanism

### Cargo and rustc JSON

`shape_shell_output` (`crates/squeezy-tools/src/shell_output.rs:28–59`)
dispatches by command family. For `cargo` and `rustc` it parses both
stdout and stderr as a stream of newline-delimited JSON documents and
keeps only the diagnostics that name an error, warning, or
`failure-note`:

```rust
// crates/squeezy-tools/src/shell_output.rs:116–187
fn parse_cargo_or_rustc_json(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let mut kept = Vec::new();
    let mut plain_lines = Vec::new();
    let mut parsed = 0usize;
    let mut finished = None;
    for line in stdout.lines().chain(stderr.lines()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // Cargo emits libtest's plain-text harness output (e.g. "test result:
            // FAILED.", panic backtraces, "FAILED" markers) interleaved with the
            // JSON stream. Preserve those signal lines so shaped output still
            // surfaces test failures.
            if libtest_signal_line(line) {
                plain_lines.push(trim_shaped_block(line.trim_end(), 4_000));
            }
            continue;
        };
        parsed += 1;
        match value.get("reason").and_then(Value::as_str) {
            Some("compiler-message") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let level = message
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("note");
                if !matches!(level, "error" | "warning" | "failure-note") {
                    continue;
                }
                let text = message
                    .get("rendered")
                    .and_then(Value::as_str)
                    .or_else(|| message.get("message").and_then(Value::as_str))
                    .unwrap_or("");
                if !text.trim().is_empty() {
                    kept.push(trim_shaped_block(text, 4_000));
                }
            }
            Some("build-finished") => {
                finished = value
                    .get("success")
                    .and_then(Value::as_bool)
                    .map(|success| format!("build-finished success={success}"));
            }
            _ => {}
        }
        // ...
    }
    if parsed == 0 {
        return None;
    }
```

Three reduction levers are doing the work here:

1. **Reason filter.** Only `compiler-message` and the terminal
   `build-finished` payloads are retained; cargo's `compiler-artifact`,
   `build-script-executed`, `manifest-path` and similar records — which
   account for the bulk of cargo's JSON volume — are dropped.
2. **Level filter.** Inside `compiler-message`, only `error`,
   `warning`, and `failure-note` levels survive. `note`, `help`, and
   `info` are dropped because the `rendered` field of the error already
   includes the relevant note/help spans inline.
3. **Plain-line carve-out.** Lines that don't parse as JSON are matched
   against `libtest_signal_line`
   (`shell_output.rs:189–212`), which keeps panic backtraces,
   `failures:` block headers, `---- name stdout ----` markers,
   and the non-empty `test result:` rows while dropping the empty
   `test result: ok. 0 passed; 0 failed` rows that cargo prints for
   every filtered-out test binary.

A subtle correctness detail: if `parsed == 0` the function returns
`None` so a plain-text-only run falls through to
`shape_unstructured_stream`. Without this guard, a libtest-only stream
would be misclassified as "structured" and lose the unstructured
shaper's dedupe accounting.

### Nextest aggregation

`parse_nextest_json` (`shell_output.rs:214–267`) treats nextest's
per-test JSON event stream as a sequence of state updates and emits a
single summary line plus the events whose `event`, `status`, or
embedded strings match `line_has_signal`:

```rust
// crates/squeezy-tools/src/shell_output.rs:214–268
fn parse_nextest_json(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let mut kept = Vec::new();
    let mut parsed = 0usize;
    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut last_summary: Option<Value> = None;
    for line in stdout.lines().chain(stderr.lines()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        parsed += 1;
        let event = value
            .get("type")
            .or_else(|| value.get("event"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        let status_lower = status.to_ascii_lowercase();
        let event_lower = event.to_ascii_lowercase();
        let is_per_test_finish = event_lower.contains("test")
            && (event_lower.contains("finish") || event_lower.contains("complete"));
        if is_per_test_finish || !status.is_empty() {
            total += 1;
            if status_lower.contains("pass") || status_lower == "ok" {
                passed += 1;
            } else if status_lower.contains("fail") || status_lower.contains("error") {
                failed += 1;
            } else if status_lower.contains("skip") || status_lower.contains("ignore") {
                skipped += 1;
            }
        }
        if event_lower.contains("summary") || event_lower.contains("run-finished") {
            last_summary = Some(value.clone());
        }
        if line_has_signal(event) || line_has_signal(status) || value_contains_signal(&value) {
            kept.push(trim_shaped_block(&value.to_string(), 4_000));
        }
    }
    if parsed == 0 {
        return None;
    }
    let mut summary_parts = vec!["family=nextest".to_string()];
    if total > 0 {
        summary_parts.push(format!(
            "total={total} passed={passed} failed={failed} skipped={skipped}"
        ));
    }
    if let Some(summary) = last_summary {
        summary_parts.push(trim_shaped_block(&summary.to_string(), 4_000));
    }
    kept.insert(0, summary_parts.join(" "));
    Some((join_shaped_lines(kept), String::new()))
}
```

The `line_has_signal` predicate (`shell_output.rs:506–517`) is the
gatekeeper for what survives:

```rust
// crates/squeezy-tools/src/shell_output.rs:506–517
fn line_has_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("warning")
        || lower.contains("fail")
        || lower.contains("panic")
        || lower.contains("status")
        || lower.contains("exit")
        || lower.contains("passed")
        || lower.contains("test result")
        || lower.starts_with("finished ")
}
```

Note the `value_contains_signal` recursion (`shell_output.rs:349–358`):
nextest sometimes embeds the failure marker deep inside a struct
(e.g. `"outcome": { "status": "fail" }`), so the predicate walks the
JSON tree rather than only inspecting the top-level `status`/`event`
fields. A passing run collapses to one line (`family=nextest
total=N passed=N failed=0 skipped=0 …`) regardless of how many tests
ran.

### Pytest, Jest, Vitest

`parse_test_report_json` (`shell_output.rs:270–283`) handles the
JS/Python test runners by parsing a single JSON document and walking
it for signal-bearing strings:

```rust
// crates/squeezy-tools/src/shell_output.rs:270–324
fn parse_test_report_json(stdout: &str, stderr: &str, family: &str) -> Option<(String, String)> {
    // jest/pytest/vitest emit a single JSON document on either stdout or
    // stderr. Combining them with a newline produces invalid JSON when both
    // streams have content (e.g. npm warnings on stderr alongside a real
    // report on stdout), so try each stream individually.
    let value = parse_first_valid_json(stdout).or_else(|| parse_first_valid_json(stderr))?;
    let mut kept = Vec::new();
    collect_json_signal_lines(&value, "$", &mut kept);
    let summary = json_test_summary(&value, family);
    if !summary.is_empty() {
        kept.insert(0, summary);
    }
    Some((join_shaped_lines(kept), String::new()))
}

fn json_test_summary(value: &Value, family: &str) -> String {
    let mut parts = vec![format!("family={family}")];
    for key in [
        "success",
        "numFailedTests",
        "numPassedTests",
        "numTotalTests",
        "failed",
        "passed",
        "total",
        "exitCode",
    ] {
        if let Some(value) = value.get(key)
            && (value.is_boolean() || value.is_number() || value.is_string())
        {
            parts.push(format!("{key}={value}"));
        }
    }
    if parts.len() == 1 {
        String::new()
    } else {
        parts.join(" ")
    }
}
```

`collect_json_signal_lines` (`shell_output.rs:326–347`) annotates each
extracted string with its JSON path (e.g.
`$.testResults[2].assertionResults[0].failureMessages[0]: ...`), which
matters because pytest's failure body and Jest's stack traces both
typically live three or four levels deep. The path tag lets the model
correlate the failure text with the file/test it came from without
having to read the full JSON.

The fallback to `parse_first_valid_json` on the *other* stream
(`shell_output.rs:285–299`) addresses a real failure mode: npm prints
deprecation warnings on stderr while Jest emits its report on stdout,
or pytest in CI flips the streams. Concatenating them would yield
invalid JSON; trying each independently recovers the report.

### Shaped block truncation

Every shaped string runs through `trim_shaped_block`:

```rust
// crates/squeezy-tools/src/shell_output.rs:519–531
fn trim_shaped_block(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.trim().to_string();
    }
    let mut output = text.chars().take(max_chars).collect::<String>();
    // Name the recovery tool so the model knows to fetch the full block
    // from the shell spillover tempfile (path lives in the structured
    // `spillover.path` field and the spillover footer below).
    output.push_str(
        "\n[truncated shaped block; recover full block via read_tool_output {\"path\": \"<spillover-path>\"}]",
    );
    output
}
```

Cargo, rustc, nextest, and the JS test runners pass `4_000`. The
unstructured shaper in `shape_unstructured_stream` passes `2_000`
(`shell_output.rs:425`). The asymmetry matches the expected information
density: a single shaped cargo diagnostic that includes the rendered
ANSI label, the source span, and the help text easily clears 2 KB; an
unstructured line almost never carries that much signal.

The recovery hint is significant — it names the tool (`read_tool_output`)
and the parameter (`path`) so the model can pivot without inferring
the contract from the surrounding chatter.

### Shell spillover

The shell tool caps in-memory capture at `output_cap` bytes and
middle-truncates. The bytes past the boundary are routed to
`ShellSpilloverStore`:

```rust
// crates/squeezy-tools/src/shell_spillover.rs:1–34
//! Per-session tempfile spillover for shell-tool output that exceeds
//! the truncation budget.
//!
//! Squeezy hard-caps its in-memory shell stdout/stderr capture at
//! `output_cap` bytes and middle-truncates the result the model sees.
//! When the capture overflows the cap, the bytes past the boundary
//! would otherwise be permanently lost — discarding the signal a long
//! build log, verbose stack trace, or other oversized output carries.
//!
//! [`ShellSpilloverStore`] preserves the captured raw stdout/stderr by
//! writing it to a per-session directory under
//! `$TMPDIR/squeezy-spillover/<session-id>/`. The shell tool surfaces
//! the path in the truncated result so the model can call
//! `read_tool_output { path }` to fetch byte ranges.

/// Per-session byte budget for tempfile spillover. 100 MB matches the
/// cap requested by the F01 finding and keeps transient disk usage
/// bounded even for very long sessions.
pub(crate) const DEFAULT_SHELL_SPILLOVER_BUDGET_BYTES: u64 = 100 * 1024 * 1024;
```

The spill path is constructed from a sha256-prefix of the payload plus
a sanitised call id, written under the session directory, and
returned as a `ShellSpilloverInfo { path, bytes }`:

```rust
// crates/squeezy-tools/src/shell_spillover.rs:115–146
pub(crate) fn spill(
    &self,
    call_id: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Option<ShellSpilloverInfo> {
    let bytes = encode_spill_payload(stdout, stderr);
    if bytes.is_empty() {
        return None;
    }
    let size = bytes.len() as u64;
    // Reserve budget atomically so concurrent shell calls cannot
    // race past the cap. Reservation is rolled back on write
    // failure to keep `bytes_used` honest.
    if !self.try_reserve(size) {
        return None;
    }
    if fs::create_dir_all(&self.session_dir).is_err() {
        self.release(size);
        return None;
    }
    let short_hash = &sha256_hex(&bytes)[..SPILL_SHORT_HASH_HEX];
    let sanitized = sanitize_call_id(call_id);
    let path = self
        .session_dir
        .join(format!("{sanitized}-{short_hash}.txt"));
    if fs::write(&path, &bytes).is_err() {
        self.release(size);
        return None;
    }
    Some(ShellSpilloverInfo { path, bytes: size })
}
```

The 100 MiB session budget bounds disk usage across an entire
agent loop. `try_reserve` (`shell_spillover.rs:175–189`) uses a
`compare_exchange` loop so concurrent shell calls can't double-count
their way past the cap. `read_range`
(`shell_spillover.rs:152–173`) canonicalises the requested path and
rejects anything outside `session_dir_canonical`
(`shell_spillover.rs:202–223`), which closes the obvious symlink and
`..`-traversal escapes. The `Drop` impl
(`shell_spillover.rs:232–239`) does best-effort `remove_dir_all` so
spillover never outlives the registry that produced it.

### Grep caps

`execute_grep` enforces three independent caps that bound how much
output a single grep can pin to the prompt:

```rust
// crates/squeezy-tools/src/file_ops.rs (Content mode)
GrepOutputMode::Content => {
    let line_text = truncate_text(line, 2_000);
    let mut next = serde_json::Map::new();
    next.insert("path".to_string(), json!(&rel_str));
    next.insert("line".to_string(), json!(line_index + 1));
    next.insert("text".to_string(), json!(line_text));
    if context > 0 {
        let before_start = line_index.saturating_sub(context);
        let before_lines: Vec<Value> = lines[before_start..line_index]
            .iter()
            .enumerate()
            .map(|(offset_idx, ctx_line)| {
                json!({
                    "line": before_start + offset_idx + 1,
                    "text": truncate_text(ctx_line, 2_000),
                })
            })
            .collect();
        // ... after_lines built symmetrically ...
        next.insert("context_before".to_string(), Value::Array(before_lines));
        next.insert("context_after".to_string(), Value::Array(after_lines));
    }
    let next = Value::Object(next);
    let next_len = serde_json::to_string(&next).map_or(0, |text| text.len());
    if cost.output_bytes + next_len as u64 > output_byte_cap as u64 {
        cost.truncated = true;
        stop_search = true;
        break;
    }
    cost.output_bytes += next_len as u64;
    cost.matches_returned += 1;
    matches.push(next);
}
```

The cap stack:

- **Per-line cap.** `truncate_text(line, 2_000)` shortens any single matched
  line or context line to 2,000 characters. Generated code,
  minified JS, and accidental binary matches can't blow the budget on
  one match.
- **Total byte cap.** `DEFAULT_OUTPUT_BYTE_CAP = 48_000`
  (`file_ops.rs:25`) is the default; callers can raise it up to 128 KB
  via `output_byte_cap`. Every prospective match is serialised, the
  serialised length is measured, and if it would push `output_bytes`
  past the cap the search short-circuits with `truncated = true`.
- **`BTreeSet` dedup of paths.** In `FilesWithMatches` mode the result
  set is a `BTreeSet<String>` (`file_ops.rs:321`), so the same path
  matched by many lines counts once and the output remains sorted —
  cheaper for the model to scan than an unsorted dup list.

The implicit ceiling: at the 48 KB default cap and ~120 bytes per
match record, a single grep returns roughly 400 matches before
truncation. That is far below the ~10,000 lines a literal `grep -rn`
on a large repo would emit.

### `diff_only` reads

Both glob and read_file accept a `diff_only` flag that restricts results
to the worktree's changed files:

```rust
// crates/squeezy-tools/src/file_ops.rs:167–172 (glob)
let include_ignored = args.include_ignored.unwrap_or(false);
let diff_only = args.diff_only.unwrap_or(false);
let diff_paths = if diff_only {
    diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()))
} else {
    BTreeSet::new()
};
```

The walker then skips any path not in `diff_paths`
(`file_ops.rs:213–216`). `read_file` enforces the same gate, but
returns a structured refusal instead of silently emptying the result:

```rust
// crates/squeezy-tools/src/file_ops.rs:526–538
if args.diff_only.unwrap_or(false) {
    let diff_paths =
        diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()));
    if !diff_paths.contains(rel_str.as_str()) {
        return make_result(
            call,
            ToolStatus::Denied,
            json!({ "error": "refusing to read a clean file because diff_only=true", "path": rel_str }),
            ToolCostHint::default(),
            None,
        );
    }
}
```

The pattern is designed for the post-edit verification loop: after the
agent has written a few files, a `diff_only=true` glob/grep/read drops
all bytes belonging to clean files. On a large workspace this is the
difference between paging in tens of thousands of unmodified lines and
paging in only the files the agent itself just touched.

### Image base64 path

`read_file` runs magic-byte detection on the policy-prefix bytes it
already had to read for the exclusion check:

```rust
// crates/squeezy-tools/src/file_ops.rs:27–47
/// Detect the canonical image MIME type from a byte prefix using magic
/// numbers. Supports PNG, JPEG, GIF (87a/89a), and WEBP (RIFF / WEBP
/// container) — the set of formats the upstream vision providers
/// (Anthropic, OpenAI, Google, Bedrock) all accept. Returns `None` when
/// the prefix does not match a known image format so the caller can
/// fall back to the default text path.
pub(crate) fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}
```

When the prefix matches, the read switches to a structured payload that
the agent wraps as `LlmInputItem::Image`:

```rust
// crates/squeezy-tools/src/file_ops.rs:558–600
// F18: detect image MIME via magic bytes on the prefix we already
// read for policy checks. PNG, JPEG, GIF, and WEBP all surface in
// the first 12 bytes, so the policy-prefix read covers detection
// without an extra syscall. When the file is an image, return a
// structured payload (path / mime / base64 data / sha256) so the
// agent can wrap the bytes in `LlmInputItem::Image` instead of
// re-serialising binary content as lossy UTF-8 text.
if let Some(mime) = prefix_bytes.as_deref().and_then(detect_image_mime) {
    let bytes = match read_range(&path, 0, total_bytes as usize) {
        Ok(bytes) => bytes,
        Err(err) => return tool_error(call, err),
    };
    let content_sha256 = match sha256_file(&path) {
        Ok(hash) => hash,
        Err(err) => return tool_error(call, err),
    };
    let data_base64 = BASE64_STANDARD.encode(&bytes);
    let encoded_len = data_base64.len() as u64;
    let mut payload = serde_json::Map::new();
    payload.insert("path".to_string(), json!(&rel_str));
    payload.insert("image".to_string(), json!(true));
    payload.insert("mime_type".to_string(), json!(mime));
    payload.insert("total_bytes".to_string(), json!(total_bytes));
    payload.insert("sha256".to_string(), json!(&content_sha256));
    payload.insert("data_base64".to_string(), Value::String(data_base64));
    // ...
    return make_result(
        call,
        ToolStatus::Success,
        Value::Object(payload),
        cost,
        Some(content_sha256),
    );
}
```

Two non-obvious cost properties: the detection reuses the prefix the
exclusion check already loaded (`POLICY_PREFIX_BYTES = 4096`, see
`crates/squeezy-tools/src/lib.rs:166`), so detection is free; and the
base64 payload is treated by the model as an image input rather than
text, which avoids the mojibake the naïve UTF-8 path would have
produced and lets the vision provider page the bytes against image
quotas instead of text quotas.

## Worked example

The model runs `cargo test --workspace`. The shell tool captures
~80 KB of stdout: cargo's compile log (~30 KB), libtest's plain-text
harness output (~15 KB), and the JSON event stream interleaved with
both (~35 KB). Three test failures and seven warnings are buried in
the JSON.

1. **Shaping dispatch.** `shape_shell_output`
   (`shell_output.rs:28`) inspects the command, sees `cargo test ...`,
   and routes to `parse_cargo_or_rustc_json`.
2. **JSON stream parse.** The parser iterates 600+ JSON lines, drops
   every `compiler-artifact`, `build-script-executed`, and informational
   record, and keeps the seven `warning` and three `error` rendered
   strings. It also walks the non-JSON lines and keeps a handful of
   libtest `failures:` markers and panic backtraces via
   `libtest_signal_line`.
3. **Shaped block trim.** Each retained `rendered` string runs through
   `trim_shaped_block(text, 8_000)`
   (`shell_output.rs:151`). One particularly verbose error span
   crosses 8,000 chars and gets a trailing `[truncated shaped block;
   recover full block via read_tool_output {"path":
   "<spillover-path>"}]` marker; the others fit.
4. **Output assembly.** `kept.push("build-finished success=false")`,
   then `plain_lines` (libtest harness rows) are appended, then
   `join_shaped_lines` glues everything into ~2 KB of text and
   `kind=structured` flows out to the shell-tool result.
5. **Spillover.** Because the raw capture exceeded `output_cap`, the
   shell tool calls `ShellSpilloverStore::spill` with the full 80 KB
   buffer (`shell_spillover.rs:115`). The store sha256-prefixes the
   payload, writes
   `$TMPDIR/squeezy-spillover/<pid>-<ts>-<n>/<call_id>-<sha16>.txt`,
   and the path is appended to the result via the spillover footer.
6. **Model reasoning.** The model sees seven warnings, three errors,
   the libtest failure markers, and the `build-finished success=false`
   line. It identifies the first error as the root cause; the third
   error was truncated mid-rendered.
7. **Recovery.** The model calls
   `read_tool_output {"path": "<spillover-path>", "offset": 0, "limit": 65536}`.
   `ShellSpilloverStore::read_range` (`shell_spillover.rs:152`)
   canonicalises the path, confirms it lives under the session dir,
   reads the window, and returns the full bytes. The model now has
   the third error's complete rendered span without ever having paid
   the 80 KB tax on the first call.

The end-to-end story is that the model paid ~2 KB on the round-trip
for the build summary, then opted into ~64 KB on the *single* call
where it actually needed the raw bytes — and only the bytes for one
error window. A naïve agent would have paid ~80 KB on the first
round-trip and gained nothing from it.

## Edge cases & limits

- **Unrecognised commands.** `shell_output_family`
  (`shell_output.rs:61–91`) only routes `cargo`, `cargo nextest`,
  `rustc`, `pytest`, `jest`, and `vitest` through structured parsers.
  Everything else returns `"shell"` and falls through to
  `shape_unstructured_stream`, which still applies head/tail capping,
  noise filtering, dedupe, and per-line trim — but the savings are
  smaller because there is no schema to filter against.
- **Structured family with no parseable output.** When `parsed == 0`
  in `parse_cargo_or_rustc_json` or `parse_nextest_json`, the function
  returns `None` and the result records
  `kind = "raw_passthrough_shaped"` along with `fallback_reason =
  "<family> structured output was unavailable or could not be parsed"`
  (`shell_output.rs:46–58`). This protects against a misclassified
  command emitting useful text but no JSON.
- **Spillover budget exhaustion.** `try_reserve` returns `false`
  once `bytes_used + size > budget_bytes` (`shell_spillover.rs:175–189`),
  and `spill` returns `None`. The result still ships with the shaped
  output but no recovery path — the model loses the ability to fetch
  the raw bytes for the remainder of the session. With a 100 MiB
  budget this is unreachable in normal use but it is the failure mode
  to watch for in extremely long agent loops.
- **Image size cap.** Image reads are explicitly capped at 5 MiB before
  base64 encoding. Larger images return an error instead of pinning a
  multi-megabyte `data_base64` payload into the response.
- **Grep dedup false negatives.** The
  `FilesWithMatches` mode dedupes by `rel_str.clone()`
  (`file_ops.rs:454`); two paths that point to the same file via
  different prefixes (rare in practice — the walker always emits one
  canonical relative path per entry) would count twice. The
  `Content` mode does not dedupe by line text, so identical matches
  from different files are kept — by design, since location is the
  thing the agent usually needs.
- **The recovery hint as token tax.** Every truncated shaped block
  appends `[truncated shaped block; recover full block via
  read_tool_output {"path": "<spillover-path>"}]` (~98 chars)
  and every truncated unstructured stream appends the longer
  `[raw stream was truncated; recover full bytes via read_tool_output
  {"path": "<spillover-path>"}]` (~96 chars). The placeholder
  `<spillover-path>` is a literal; the actual path is appended later
  by `append_spillover_footer` in `shell.rs` and lives in the
  structured `spillover.path` field. The hint is small enough
  relative to the bytes saved to be unambiguously worth it.
- **Diff snapshot cost.** `diff_only=true` triggers
  `self.diff_snapshot(DiffMode::Worktree, ...)`
  (`file_ops.rs:168–172, 527–528`). The diff is recomputed per call;
  on a large worktree this is non-trivial CPU work, but the savings
  on the read budget dwarf the cost in any reasonable scenario.

## Cost intuition

Putting per-shaper compression rates next to where each shaper
contributes most:

- **Cargo / rustc JSON shaping.** Typical reduction is 95–98% on
  successful builds (essentially everything except `build-finished`
  is dropped) and 80–95% on failing builds (the diagnostics
  themselves dominate the kept set). A 200 KB build log routinely
  collapses to 2–8 KB of `rendered` text. The shaper is the single
  highest-leverage filter in the system because cargo's progress
  chatter is repetitive and the JSON event stream is heavily padded
  with `compiler-artifact` records.
- **Nextest aggregation.** Reduction is around 90% on a passing run
  (everything but the one summary line is dropped) and ~70–80% on a
  failing run (per-test JSON events fire for every test, but only the
  failures clear `line_has_signal`). A 50 KB nextest log collapses to
  5–15 KB.
- **Pytest / Jest / Vitest.** Reduction varies more with how much
  the test report nests failure text; in practice a passing run
  collapses to one summary line (>99% reduction) and a failing run
  collapses to one summary line plus a handful of
  `$.testResults[i].assertionResults[j].failureMessages[k]` rows
  (typically 80–95% reduction).
- **Unstructured shell shaping.** On commands with heavy progress
  output (npm install, docker build, terraform plan) the head/tail
  cap with noise filtering and `[repeated previous line N more times]`
  collapse routinely hits 70–90%. On commands that emit short
  unique output (most one-shot CLIs), the reduction is small but
  the per-line cap and dedupe still bound the worst case.
- **Grep.** The combination of 2000 char/line + 48 KB total cap +
  `BTreeSet` path dedup gives 70–90% reduction versus running the
  equivalent ripgrep query unconstrained. The cap is hit most often
  on common identifiers; rare-identifier searches return below the
  cap and pay nothing for the limits.
- **`diff_only` reads.** Reduction depends entirely on the size of
  the worktree diff relative to the workspace. In the post-edit
  verification loop the diff is typically <1% of the workspace, so
  the reduction is 99%+ on whatever read/glob/grep the agent issues
  with the flag set.
- **Image base64.** The base64 payload is roughly 1.33× the binary
  size, so it is technically *larger* than the raw bytes. The win
  is structural: routing through `LlmInputItem::Image` lets the
  provider handle the bytes as an image (with its own caching and
  size budgets) instead of as multi-megabyte mojibake competing for
  the text context window.

Aggregated across an agent loop that issues dozens of shell calls,
tens of greps, and the occasional file read, structured tool-output
extraction is the largest single source of token savings after
prompt caching. The asymmetry holds because every tool call is a
fresh stream — the savings compound with loop length, where prompt
caching only compounds with conversation length. The two stack
cleanly: the shapers reduce what enters the prompt, the cache
reduces what gets re-billed once it's in.
