# Tool-Output Deduplication and Receipt Stubs

## Motivation

Coding agents are bursty re-readers. A model that already pulled `Cargo.toml`
in turn 3 frequently pulls it again in turn 7. The same shape shows up with
`grep` (a dozen near-identical pattern searches returning mostly-overlapping
match sets) and worse with `webfetch` (the same docs page four or five times
across a multi-turn task). Every re-send of identical bytes is pure waste:
the model paid input tokens for those bytes once; the provider does not
refund the second time.

Squeezy's dedup pipeline replaces every byte the model has already seen — in
this conversation or any prior conversation in the same workspace — with a
small pointer the next time it would be sent. The pipeline is layered: three
layers in `crates/squeezy-agent/src/context_compaction.rs`, one in the tool
itself (`crates/squeezy-tools/src/file_ops.rs`), persistence in
`crates/squeezy-store`.

## Mechanism

### Layer 1 — In-conversation dedup (`context_compaction.rs:1293–1615`)

Every batch of tool results, after the tools run but before the results are
appended to the conversation, passes through `SeenToolOutputs::prepare_results`
(line 1333). That method walks the batch and, for each `ToolResult`, checks
whether the model has already seen an output with the same `(tool_name,
stable_output_sha256)` key. If yes, the original `ToolResult` is replaced with
a receipt stub. If no, the result is preserved and remembered.

The eligible-tools allowlist is hard-coded in `is_receipt_stub_candidate`
(line 1422). Only tools whose outputs are content-addressable and idempotent
qualify:

```rust
// crates/squeezy-agent/src/context_compaction.rs:1422-1442
fn is_receipt_stub_candidate(result: &ToolResult) -> bool {
    result.status == ToolStatus::Success
        && matches!(
            result.tool_name.as_str(),
            "decl_search"
                | "definition_search"
                | "downstream_flow"
                | "glob"
                | "grep"
                | "hierarchy"
                | "read_file"
                | "read_slice"
                | "read_tool_output"
                | "reference_search"
                | "repo_map"
                | "symbol_context"
                | "upstream_flow"
                | "webfetch"
                | "websearch"
        )
}
```

Mutators (`run_shell`, `apply_diff`) and anything whose output is timestamped
are deliberately excluded. A `grep` returning the same matches twice dedups;
a `run_shell` running `date` would not.

The equality key is `stable_output_sha256` (line 1444), a three-tier
fallback: a `cache_receipt.stable_output_sha256` field that tools may
populate over a normalized payload, a top-level `original_output_sha256`,
and finally the full-output SHA from `result.receipt.output_sha256`:

```rust
// crates/squeezy-agent/src/context_compaction.rs:1444-1458
fn stable_output_sha256(result: &ToolResult) -> String {
    result
        .content
        .get("cache_receipt")
        .and_then(|value| value.get("stable_output_sha256"))
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .content
                .get("original_output_sha256")
                .and_then(Value::as_str)
        })
        .unwrap_or(&result.receipt.output_sha256)
        .to_string()
}
```

`read_file` hashes raw file bytes and skips envelope fields like
`path`/`offset`/`bytes_returned`, so two reads of the same window dedup even
if the wrapping JSON differs by call id. Tools that don't opt in fall back
to byte-for-byte equality on the wrapped JSON. On a hit,
`receipt_stub_result` (line 1493) replaces `content`:

```rust
// crates/squeezy-agent/src/context_compaction.rs:1493-1522
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
    // ... rebuilds ToolResult with stub content + truncated cost_hint
}
```

The `same_as_call_id` pointer is load-bearing: it tells the model exactly
which earlier call carries the bytes, so it can scroll back if needed. The
stub also carries the SHA256 of the original output, the SHA256 of the
underlying content (for `read_file`, the file's content hash), and the size
of the omitted payload so the model can decide whether re-fetching is worth
the bytes.

The `negative_receipt_stub` flag (`is_negative_receipt_result`, line 1524) is
set when the deduplicated result was an empty `grep`/`glob`: the model is
told explicitly "the repeat search still returned nothing."

### Layer 2 — Cross-session receipts (`SqueezyStore::put_tool_receipt`)

`SeenToolOutputs::remember_results` (line 1391) persists each new
`SeenToolOutput` to disk through the store, not just to the in-memory
`BTreeMap`:

```rust
// crates/squeezy-agent/src/context_compaction.rs:1391-1413
pub(crate) fn remember_results(&mut self, results: &[PendingToolResult]) {
    for result in results {
        if let Some(seen) = result.remember.clone() {
            self.by_tool_output
                .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                .or_insert(seen.clone());
            if let Some(store) = self.store.as_deref() {
                let _ = store.put_tool_receipt(&StoredToolReceipt {
                    tool_name: seen.tool_name.clone(),
                    stable_output_sha256: seen.stable_output_sha256.clone(),
                    call_id: seen.call_id.clone(),
                    content_sha256: seen.content_sha256.clone(),
                    model_output_bytes: seen.model_output_bytes,
                    created_unix_millis: unix_millis(),
                    summary: seen.summary.clone(),
                });
                if let Some(snap) = read_snapshot_from_result(&result.result, &seen) {
                    let _ = store.put_read_snapshot(&snap);
                }
            }
        }
    }
}
```

`SeenToolOutputs::from_store` (line 1307) is the inverse: on startup it
pulls every prior receipt out of the store and pre-loads the in-memory map.
A brand-new session opens with the dedup table populated from every prior
session in that workspace.

The store is a redb database. Tables at `crates/squeezy-store/src/lib.rs:46`:

```rust
// crates/squeezy-store/src/lib.rs:46-47, 855-857
const TOOL_RECEIPTS: TableDefinition<&str, &[u8]> = TableDefinition::new("tool_receipts");
const READ_SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("read_snapshots");
fn receipt_key(tool_name: &str, stable_output_sha256: &str) -> String {
    format!("{tool_name}\0{stable_output_sha256}")
}
```

Receipts are globally keyed by `(tool_name, stable_output_sha256)` within a
workspace. Two sessions that grep for the same pattern and get the same hits
collide on one key; `insert_json` overwrites but the dedup result is
identical either way. The stored payload:

```rust
// crates/squeezy-store/src/lib.rs:658-668
pub struct StoredToolReceipt {
    pub tool_name: String,
    pub stable_output_sha256: String,
    pub call_id: String,
    pub content_sha256: Option<String>,
    pub model_output_bytes: usize,
    pub created_unix_millis: u128,
    #[serde(default)] pub summary: Option<String>,
}
```

`model_output_bytes` is what the dedup saved — the size sent to the model
the first time. `summary` is a one-line description from
`tool_result_summary` (`crates/squeezy-agent/src/lib.rs:12331`); it seeds the
compaction summary via `receipt_summary_lines` (line 1236), so the knowledge
persists even when the receipt is later dropped from context.

For `read_file` and `read_slice`, `read_snapshot_from_result` (line 1460)
also writes a `StoredReadSnapshot` keyed by `(path, start_byte, end_byte)`
carrying the actual content bytes. This lets the per-tool dedup in Layer 4
short-circuit before any I/O.

### Layer 3 — Aggregate per-round budget (`pack_tool_results`)

Dedup handles repetition; it does not bound fan-out. If a model issues
fourteen unique `grep` calls in one round, none dedupe — they all hit the
conversation. `pack_tool_results` walks the post-dedup batch with a running
byte counter:

```rust
// crates/squeezy-agent/src/context_compaction.rs:1548-1591
pub(crate) fn pack_tool_results(
    results: Vec<PendingToolResult>,
    budget_bytes: usize,
) -> Vec<PendingToolResult> {
    if budget_bytes == 0 {
        return results;
    }

    let mut used = 0usize;
    let mut visible_current_call_ids = BTreeSet::new();
    results.into_iter().map(|mut pending| {
        if pending.same_as_current_call_id.as_ref()
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
            let compact = pending.result.aggregate_budget_exceeded(budget_bytes, bytes);
            used = used.saturating_add(compact.model_output().len());
            PendingToolResult { result: compact, remember: None, same_as_current_call_id: None }
        }
    }).collect()
}
```

Three properties: (1) **FIFO, not size-weighted** — the first result that
pushes `used` over `budget_bytes` is truncated and every subsequent result
is too, with no priority reordering. (2) **Truncation stubs are budgeted**
via `used.saturating_add(compact.model_output().len())`, preventing
pathological re-stubbing. (3) **Stale receipt-stub pointers get rewritten:**
if B is a stub pointing at A and A gets truncated, `visible_current_call_ids`
catches the orphan and `receipt_stub_reference_omitted` (line 1593) replaces
it:

```rust
// crates/squeezy-agent/src/context_compaction.rs:1593-1614
fn receipt_stub_reference_omitted(result: ToolResult) -> ToolResult {
    let content = json!({
        "error": "tool result omitted because the identical result it references was omitted by the aggregate tool-result budget",
    });
    // ... wrap as Error ToolResult
}
```

The truncation payload is built by `ToolResult::aggregate_budget_exceeded`
at `crates/squeezy-tools/src/lib.rs:743`:

```rust
// crates/squeezy-tools/src/lib.rs:743-765
pub fn aggregate_budget_exceeded(&self, budget_bytes: usize, actual_bytes: usize) -> Self {
    make_result(/* call rebuilt from self */, ToolStatus::Error,
        json!({
            "error": "tool result omitted because aggregate tool-result budget was exceeded",
            "budget_bytes": budget_bytes,
            "actual_bytes": actual_bytes,
            "original_status": &self.status,
            "original_output_sha256": self.receipt.output_sha256,
        }),
        ToolCostHint { truncated: true, ..Default::default() },
        self.receipt.content_sha256.clone(),
    )
}
```

`original_output_sha256` lets the model recover bytes via `read_tool_output`
against the spill cache (covered in the spill chapter). The discoverability
hint `"Full output spilled to disk. Use read_tool_output with handle
{sha256}..."` lives at `lib.rs:4225`.

The budget is `config.max_tool_result_bytes_per_round`
(`crates/squeezy-core/src/lib.rs:392`), default **50,000 bytes**
(`DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND`, line 197). Env override:
`SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND`. The packer runs at three call
sites: `crates/squeezy-agent/src/lib.rs:5082`, `5884`, `8026` — main turn
loop, subagent loop, eval harness.

### Layer 4 — Per-tool dedup at the tool itself (`file_ops.rs:605–651`)

Layers 1–3 act on `ToolResult` values after the tool has done its work. For
`read_file`, that means hashing the file, slicing the window, and serializing
UTF-8 into JSON. If the model is going to re-read the same window, even
producing the result is wasteful. `read_file` has an opt-in short-circuit:

```rust
// crates/squeezy-tools/src/file_ops.rs:605-651
// F03: dedup against the last receipt for this (path, offset, end) window.
let content_sha256 = match sha256_file(&path) {
    Ok(hash) => hash,
    Err(err) => return tool_error(call, err),
};
let projected_end = offset.saturating_add(limit).min(total_bytes as usize);
if let Some(store) = self.state_store.as_deref()
    && let Ok(snapshots) = store.read_snapshots_for_path(rel_str.as_str())
{
    let prior = snapshots
        .iter()
        .filter(|snap| snap.start_byte == offset as u64
            && snap.end_byte == projected_end as u64
            && snap.tool_name == "read_file")
        .filter(|snap| snap.content_sha256.as_deref() == Some(content_sha256.as_str()))
        .max_by_key(|snap| snap.created_unix_millis);
    if let Some(snap) = prior {
        return make_result(call, ToolStatus::Success,
            json!({
                "tool": "read_file", "path": &rel_str, "offset": offset,
                "bytes_returned": 0, "total_bytes": total_bytes,
                "sha256": &content_sha256, "unchanged": true,
                "receipt_stub": true, "dedup": true,
                "same_as_call_id": snap.call_id,
                "same_as_tool_name": snap.tool_name,
                "original_output_sha256": snap.stable_output_sha256,
                "original_content_sha256": snap.content_sha256,
                "original_model_output_bytes": snap.model_output_bytes,
                "truncated": false,
            }),
            ToolCostHint::default(), Some(content_sha256.clone()));
    }
}
```

Logic: hash the file, look for a snapshot keyed by `(path, start_byte,
end_byte)`, verify `content_sha256` still matches, emit a stub without ever
calling `read_range`. The disk I/O for the file slice and UTF-8 conversion
are both skipped.

The snapshot store is the same one Layer 2 populates. Snapshots are written
whenever `read_snapshot_from_result` (line 1460) sees a `read_file` or
`read_slice` success that is not a diff-mode read. Layer 2 records,
Layer 4 consumes on the next read.

## Worked example

### Scenario A: `read_file("Cargo.toml")` twice in three turns

**Turn 1.** Model calls `read_file({"path": "Cargo.toml"})`. `file_ops.rs`
hashes the file, queries `read_snapshots_for_path("Cargo.toml")` (no prior
snapshot), reads the bytes, builds a normal result. The agent loop runs
`prepare_results` — key `("read_file", "<sha-of-stable-output>")` is not
seen, the result passes through. `remember_results` writes a
`StoredToolReceipt` keyed `read_file\0<sha>` and a `StoredReadSnapshot`
keyed `Cargo.toml\000000000000000000000\000000000000000003472` (zero-padded
offsets, `read_snapshot_key`, `lib.rs:864`). The model receives the full
file in a ~3.5KB JSON envelope.

**Turn 2** is some unrelated grep round.

**Turn 3.** Model calls `read_file({"path": "Cargo.toml"})` again. Layer 4
wins: `file_ops.rs:610` hashes the file (same hash), `read_snapshots_for_path`
returns the snapshot from Turn 1, the window and `content_sha256` match.
`read_file` returns immediately with:

```json
{"tool": "read_file", "path": "Cargo.toml", "offset": 0,
 "bytes_returned": 0, "total_bytes": 3472, "sha256": "<file-sha>",
 "unchanged": true, "receipt_stub": true, "dedup": true,
 "same_as_call_id": "call_018xx-turn1", "same_as_tool_name": "read_file",
 "original_output_sha256": "<stable-sha>",
 "original_content_sha256": "<file-sha>",
 "original_model_output_bytes": 3641, "truncated": false}
```

Layer 1 sees this stub in `prepare_results`. The result is still `Success`
so `is_receipt_stub_candidate` returns true, but `stable_output_sha256`
extracts the original hash from `content.original_output_sha256` — which
matches the receipt already in the map. Layer 1 would have stubbed this if
Layer 4 hadn't. Either way the model sees ~250 bytes instead of ~3.5KB.

The `same_as_call_id` is the recovery hatch: the model can call
`read_tool_output` with that id to retrieve the original bytes. In practice
it almost never does.

### Scenario B: 12 grep calls in one round, 200KB aggregate cap

Suppose `max_tool_result_bytes_per_round` is bumped to 200,000 (default
50,000) and the model issues twelve `grep` calls in parallel, each
returning between 5KB and 80KB. The packer walks them:

| # | Bytes | Cumulative | Action |
|---|---|---|---|
| 1–7 | 18.4K, 6.2K, 81K, 9.8K, 12.3K, 44.5K, 7.1K | 179,300 | pass through |
| 8 | 35,000 | 179,300 + 35,000 > 200,000 | **truncated** |
| 9–12 | each | over | **truncated** |

From #8 onward, every result is replaced by:

```json
{
  "error": "tool result omitted because aggregate tool-result budget was exceeded",
  "budget_bytes": 200000,
  "actual_bytes": 35000,
  "original_status": "Success",
  "original_output_sha256": "<sha256>"
}
```

The model sees seven full match sets and five truncation stubs. To recover,
it issues `read_tool_output` against the spill handles
(`maybe_spill`, `squeezy-tools/src/lib.rs:4183`). The common case — the
model never needed the truncated outputs — costs nothing extra; the rare
case forces a deliberate follow-up call rather than dumping a megabyte into
the next turn.

If any of these results had been receipt stubs from earlier turns and the
target they pointed at also got truncated, the orphan-pointer rewrite at
line 1566 converts them into the "reference omitted" error stub.

## Edge cases and limits

**Eligible-tools allowlist.** Sixteen tools opt in (line 1422): the search
family (`decl_search`, `definition_search`, `reference_search`, `grep`,
`glob`, `repo_map`, `symbol_context`, `hierarchy`, `downstream_flow`,
`upstream_flow`), the read family (`read_file`, `read_slice`,
`read_tool_output`), and the web family (`webfetch`, `websearch`). Absent:
mutators (`run_shell`, `apply_diff`, `apply_patch`) and any tool returning
live system state. The `Success` filter also excludes errors and denials.

**What counts as identical.** Equality is `(tool_name,
stable_output_sha256)`. The three-tier fallback (line 1444) lets tools opt
into header-skipping via `cache_receipt.stable_output_sha256`; `read_file`
gets stronger equality via `read_snapshots` keyed by `content_sha256`.
Tools without opt-in fall back to byte-for-byte equality on the wrapped
JSON — conservative on purpose, no near-miss dedup.

**Negative receipts.** `is_negative_receipt_result` (line 1524) flags
empty `grep`/`glob` so the stub carries `negative_receipt_stub: true`. The
model is told the negative result is the same as before, not ambiguous
with "we never did the search."

**Receipt store keying.** Receipts: `tool_name\0stable_output_sha256`
(`crates/squeezy-store/src/lib.rs:855`). Read snapshots:
`path\0<start_byte:020>\0<end_byte:020>` (line 864). Zero-padded offsets
give `read_snapshots_for_path` a lexicographic scan matching numeric order.
Multiple windows of one file coexist; same-window `put_read_snapshot`
overwrites, so each window holds the most recent observation.

**Retention.** `tool_output_retention_days`
(`crates/squeezy-core/src/lib.rs:393`, default 7) bounds the on-disk spill
cache. The redb receipt/snapshot tables are not swept and accumulate until
the workspace's redb file is removed. Safe because they are content-hashed
— stale entries cost bytes, never correctness.

**Budget packer policy.** FIFO over the post-dedup batch. Once `used +
bytes > budget`, every subsequent result is truncated; the truncation
payload itself is budgeted. No size-weighted picking, no per-tool quota.
Empirical justification: most rounds either fit easily or blow up on a
single fan-out; reordering doesn't change the dominant case.

**Inter-layer race.** Layer 4 stubs carry `original_output_sha256`, which
Layer 1's `stable_output_sha256` picks up. The two layers agree on the key,
so no double-stubbing.

**Spill interaction.** `maybe_spill` runs before agent-level dedup. A huge
result spills to disk and returns a preview; subsequent identical fetches
dedup the preview. Worst case: one payload on disk, one preview in
conversation, every refetch a ~250-byte stub.

## Cost intuition

Empirical re-read rate in agentic loops is typically 20–40% — of N tool
calls, N/5 to N/2.5 are refetches. The rate trends up with task length:
effective working memory degrades faster than the propensity to re-check.

Per-dedup savings, rough:

- `read_file` of a 5 KB source: ~5,200 bytes → ~250-byte stub, ~20×.
- `grep` returning 40 matches: ~12 KB → ~250-byte stub, ~50×.
- `webfetch` of a docs page: ~80 KB → ~250-byte stub, ~320×.
- Negative `grep`: ~200 bytes → ~280-byte negative stub. Negative ratio in
  bytes; the value here is semantic clarity, not byte savings.

A 25% re-read rate translates to roughly 15–25% input-token savings on
multi-turn coding tasks. The aggregate budget at 50 KB caps the worst case
at ~13K tokens per round of tool output, so a thirty-grep fan-out cannot
inflate one turn's input by 500 KB.

Layer 2 extends savings across the user's session boundary: a fresh
conversation in the same workspace inherits every prior receipt, so the
first `read_file` of `Cargo.toml` in the new session may already be a stub.
Materially relevant for workflows of many short sessions against one repo.
