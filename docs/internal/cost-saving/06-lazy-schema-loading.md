# Lazy Schema Loading

## Motivation

The Anthropic Messages API and OpenAI-compatible chat APIs ship the full
JSON schema for every advertised tool on every request. Squeezy's first-party
surface alone is roughly two dozen tools: bounded I/O (`read_file`,
`read_slice`, `write_file`), structured editing (`apply_patch`,
`plan_patch`), search (`grep`, `glob`), graph navigation (`decl_search`,
`definition_search`, `diff_context`, `downstream_flow`, `hierarchy`,
`reference_search`, `repo_map`, `symbol_context`, `upstream_flow`),
shell, and a heavier tail (`webfetch`, `websearch`, `verify`,
`notebook_edit`, `notes_remember`, `list_skills`, `load_skill`,
`refresh_compiler_facts`). A handful of synthetic control tools
(`delegate`, `explore`, `delegate_plan`, `delegate_review`,
`delegate_chain`, `request_user_input`, `load_tool_schema`) sit on top,
and MCP servers can register an arbitrary tail of `mcp__*` tools beyond
that.

Each tool spec carries a description and a JSON Schema object describing
its parameters. The heaviest specs — `shell` with its sandbox arguments,
`apply_patch` with a multi-line diff payload, `plan_patch` with a
plan-binding contract, `delegate_chain` with its `{previous}`-templated
steps — run hundreds to ~1500 bytes apiece once serialized. With 30
advertised tools the full bundle is typically 30-80KB on the wire, every
turn, before a single character of conversation history has been added.

In a typical turn that bundle is dead weight: a model that is reading
two files and writing a patch needs `read_file`, `read_slice`, `grep`,
`glob`, and `apply_patch`; the full schema for `webfetch`, `websearch`,
`verify`, `delegate_chain`, and the graph navigators is sent and billed
but never invoked.

Skill bodies (Markdown instruction blobs discovered from
`.squeezy/skills/*/SKILL.md`, compatibility `.agents/skills/*/SKILL.md`,
configured extra roots, and bundled skills) are the same problem one
layer up. Once a skill is *active* for a turn, its full body can be
spliced into the system prompt; half a dozen active skills can add
another 10-30KB of instructions that the model glances at and ignores.
The legacy `[skills] inline = true` knob forces that behaviour; the
modern default emits a metadata stub and lets the model fetch the body
on demand via the `load_skill` tool.

The lazy-schema-loading subsystem makes both costs opt-in: the model
sees a short index of *what exists*, and only the schemas or bodies it
explicitly asks for end up on the wire.

## Mechanism

### Tool schema deferral

The control-flow split lives in `crates/squeezy-agent/src/lib.rs`. Three
constants name the always-on control tools:

```rust
// crates/squeezy-agent/src/lib.rs:111-114
const TASK_STATE_TOOL_NAME: &str = "update_task_state";
const LOAD_TOOL_SCHEMA_TOOL_NAME: &str = "load_tool_schema";
const DELEGATE_TOOL_NAME: &str = "delegate";
const EXPLORE_TOOL_NAME: &str = "explore";
```

The default "core" tool set — the ones whose schemas are always sent —
is declared in `squeezy-core` and is intentionally short. The doc
comment is explicit about the contract:

```rust
// crates/squeezy-core/src/lib.rs:321-356
/// Tools whose full JSON schema is always sent up-front in every request,
/// independent of `[tools].lazy_schema_loading`.
///
/// These are the cheap-and-likely-needed-every-turn tools: bounded file
/// reads/writes, structured patching, search, shell, and graph-backed navigation. Heavyweight
/// or rarely-used tools (e.g. `verify`, `webfetch`, `websearch`) are
/// intentionally **not** in this list so they only cost prompt bytes once
/// the model explicitly attaches them via `load_tool_schema`.
///
/// `load_tool_schema` is not duplicated here on purpose: it is forced into the
/// request `tools` array by name in `squeezy_agent::request_tool_specs`, and
/// `squeezy_agent::tool_is_core_schema` treats it as always-core. Listing it
/// in two places risks future skew if one site is updated without the other.
///
/// `update_task_state` is intentionally omitted from model-visible schemas.
/// The runtime derives visible progress from turn/tool lifecycle events.
pub const DEFAULT_CORE_TOOL_NAMES: &[&str] = &[
    "glob", "grep", "read_file", "read_tool_output", "write_file",
    "apply_patch", "shell", "decl_search", "definition_search",
    "diff_context", "downstream_flow", "hierarchy", "plan_patch",
    "read_slice", "reference_search", "repo_map", "symbol_context",
    "upstream_flow",
];
```

Eighteen tools — plus the synthetic control tools and
`load_tool_schema` itself — are the "core". Everything else (`verify`,
`webfetch`, `websearch`, `notebook_edit`, `notes_remember`,
`list_skills`, `load_skill`, every `mcp__*` server tool) is
*discoverable*: it shows up by name in the index but not in the request
`tools` array until the model asks for it. The runtime switch is one
flag on `ToolSchemaConfig` (`lazy_schema_loading: true` by default,
crates/squeezy-core/src/lib.rs:3120-3138).

The synthetic `load_tool_schema` tool is the protocol bridge — a small
spec (~400 bytes serialized) advertised whenever lazy loading is on
(`load_tool_schema_advertised_tool`,
crates/squeezy-agent/src/lib.rs:11326-11353). It takes one required
string `name` parameter and carries `PermissionCapability::Read` so it
is not routed through the permission engine.

`request_tool_specs` (crates/squeezy-agent/src/lib.rs:11664-11705) is
called once per LLM round. When `lazy_schema_loading` is off it returns
every advertised tool. When it is on it walks the synthetic control
tools, then the configured-core list, then the session-loaded names:

```rust
// crates/squeezy-agent/src/lib.rs:11697-11703
        .chain(schema_config.core.iter().map(String::as_str))
    {
        push_tool_spec_by_name(tools, name, mode, plan_edit_allowed, &mut specs, &mut seen);
    }
    for name in loaded_tool_schemas {
        push_tool_spec_by_name(tools, name, mode, plan_edit_allowed, &mut specs, &mut seen);
    }
    specs
}
```

The `<tools_index>` is a sibling that renders the *discoverable* set
into the system instructions. The opener and closer are constants and
the rows are sorted alphabetically for byte-stability:

```rust
// crates/squeezy-agent/src/lib.rs:11768-11802
const TOOLS_INDEX_OPENER: &str = "<tools_index>\nDiscoverable tools are listed below with compact metadata. Use load_tool_schema before calling one of these tools.\n";
const TOOLS_INDEX_CLOSER: &str = "\n</tools_index>";

fn tool_schema_index(
    tools: &[AdvertisedTool],
    mode: SessionMode,
    schema_config: &ToolSchemaConfig,
    plan_edit_allowed: bool,
) -> Option<String> {
    if !schema_config.lazy_schema_loading {
        return None;
    }
    let mut rows = tools
        .iter()
        .filter(|tool| {
            !mode_refuses_capability(mode, tool.capability, plan_edit_allowed)
                && !tool_is_core_schema(tool, schema_config)
        })
        .map(|tool| {
            format!(
                "- {} | capability={} | {}",
                tool.spec.name,
                tool.capability.as_str(),
                first_line_of_description(&tool.spec.description)
            )
        })
        .collect::<Vec<_>>();
    // Alphabetic ordering ... keeps the rendered `<tools_index>` byte-stable
    // across rounds ... The Anthropic provider marks the last first-party
    // tool definition with `cache_control: ephemeral`, so byte-stable tool
    // specs are load-bearing for that prefix cache as well.
    rows.sort();
    ...
```

Each row is `- {name} | capability={cap} | {first-line-of-description}`
— typically 60-120 bytes. Twenty-five discoverable tools cost roughly
2KB; twenty-five full schemas would be 20-60KB.

The classification rule for what is core vs. discoverable is a small
explicit predicate:

```rust
// crates/squeezy-agent/src/lib.rs:11852-11864
fn tool_is_core_schema(tool: &AdvertisedTool, schema_config: &ToolSchemaConfig) -> bool {
    let name = tool.spec.name.as_str();
    if matches!(
        name,
        DELEGATE_TOOL_NAME | EXPLORE_TOOL_NAME | LOAD_TOOL_SCHEMA_TOOL_NAME
    ) {
        return true;
    }
    if !schema_config.lazy_schema_loading {
        return true;
    }
    schema_config.core_contains(name)
}
```

When the model emits `load_tool_schema("…")`,
`handle_load_tool_schema_call` (crates/squeezy-agent/src/lib.rs:6989-7079)
validates the name against `all_tool_specs`, short-circuits to
`already_attached` if the name is core or already loaded, and otherwise
pushes the name onto a single `Vec<String>`:

```rust
// crates/squeezy-agent/src/lib.rs:7054-7078
    let mut loaded = context.loaded_tool_schemas.lock().await;
    if let Some(position) = loaded.iter().position(|loaded_name| loaded_name == name) {
        return control_tool_result(call, ToolStatus::Success,
            json!({ "ok": true, "name": name, "status": "already_attached", "position": position }));
    }
    loaded.push(name.to_string());
    let position = loaded.len() - 1;
    control_tool_result(call, ToolStatus::Success,
        json!({ "ok": true, "name": name, "status": "attached", "position": position }))
```

That `Vec<String>` is a single per-session list
(`loaded_tool_schemas: Arc<Mutex<Vec<String>>>`,
crates/squeezy-agent/src/lib.rs:1174), initialised empty at construction
(lib.rs:1594) and cloned through every turn runtime, subagent dispatch
context, and per-tool execution context.

### Skills

`LoadedSkill::metadata_block` is the skill-body counterpart of the
`<tools_index>` row. It emits the same outer `<skill>` element as the
inline `prompt_block` but replaces `<content>` with an `<instruction>`
line that tells the model how to fetch the body:

```rust
// crates/squeezy-skills/src/lib.rs:299-339
    /// Metadata-only counterpart to [`Self::prompt_block`].
    ///
    /// Emits the same outer `<skill>` shape (name, source, description,
    /// optional `when_to_use`, `location`, `base_directory`, manifest)
    /// but omits the skill body. A short `<instruction>` tells the model
    /// to call `load_skill` when the full instructions are needed. This
    /// is the default rendering path for active skills; the legacy
    /// inline-body form is gated behind `[skills] inline = true`.
    pub fn metadata_block(&self) -> String {
        ...
        let instruction = format!(
            "Skill body omitted; call load_skill with name \"{}\" to load the full instructions.",
            name
        );
        format!(
            "<skill name=\"{}\" source=\"{}\" body=\"omitted\">\n<description>{}</description>{when_to_use}\n<location>{}</location>\n<base_directory>{}</base_directory>{manifest_block}\n<instruction>{}</instruction>\n</skill>",
            ...
        )
    }
```

`SkillCatalog::load` is the cache-aware loader: a first call reads
`SKILL.md` off disk and parses its frontmatter and body, then stores the
result in a per-catalog `Mutex<BTreeMap<String, LoadedSkill>>`:

```rust
// crates/squeezy-skills/src/lib.rs:510-534
    pub fn load(&self, name: &str) -> Result<LoadedSkill> {
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(name)
        {
            return Ok(cached.clone());
        }
        let Some(entry) = self.skills.get(name) else {
            return Err(SqueezyError::Tool(format!("skill not found: {name}")));
        };
        if entry.summary.disabled {
            return Err(SqueezyError::Tool(format!("skill disabled: {name}")));
        }
        let content = fs::read_to_string(&entry.summary.location)?;
        let (metadata, body) = parse_skill_file(&content).map_err(SqueezyError::Tool)?;
        let loaded = LoadedSkill {
            summary: entry.summary.clone(),
            base_dir: entry.base_dir.clone(),
            body,
            hooks: metadata.hooks,
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(name.to_string(), loaded.clone());
        }
        Ok(loaded)
    }
```

The catalog decides at render time whether to emit metadata blocks or
the full body, gated by the `inline` flag:

```rust
// crates/squeezy-skills/src/lib.rs:574-588
    pub fn render_active_skills(&self, skills: &[LoadedSkill]) -> Option<String> {
        if self.inline {
            // Legacy behavior: inline each activated skill's full body
            // into the system prompt, with budget-aware stub fallback.
            render::render_active_skills(
                skills,
                self.active_budget_chars,
                self.active_body_cap_chars,
            )
        } else {
            // Default behavior: emit metadata-only blocks. The model
            // calls `load_skill` when it needs the body.
            render::render_active_skills_metadata(skills, self.active_budget_chars)
        }
    }
```

`render_active_skills_metadata` emits a `metadata_block` per skill and
drops the lowest-priority survivor (with a `tracing::warn!`) if the
aggregate exceeds the active budget. The budget is enforced in
characters of *bundle output*, not characters of the bodies that would
have been inlined, so metadata mode keeps eight skills active in roughly
the byte budget that inline mode would burn on two.

The model-facing `load_skill` spec advertises a single string `name`
parameter and is explicit about not changing permissions
(`load_skill_spec`, crates/squeezy-tools/src/specs.rs:561-577). Its
handler (`execute_load_skill`, crates/squeezy-tools/src/lib.rs:2667-2690)
returns the cached body verbatim in a `content` field on the tool
result. The body lives in *one* tool-output item in the conversation
buffer instead of *every* system prompt for the rest of the session.

## Worked example

Open a fresh session against the default config. The first turn's
request looks like:

- System instructions (prompt + AGENTS.md + user memory + active skill
  metadata stubs) ending with the `<tools_index>` block.
- A request `tools` array of:
  - The three synthetic control tools that pass through
    (`delegate`, `explore`, `load_tool_schema`) — ~1.5KB.
  - The eighteen `DEFAULT_CORE_TOOL_NAMES` tools — ~6KB.
- Total: roughly 7.5KB of tool schemas plus a ~2KB `<tools_index>`
  listing ~25 discoverable tools by name and one-line description.

A representative slice of the index:

```
<tools_index>
Discoverable tools are listed below with compact metadata. Use load_tool_schema before calling one of these tools.
- list_skills | capability=Read | List locally discovered Squeezy skills by metadata only. ...
- load_skill | capability=Read | Load one locally discovered skill body ...
- notebook_edit | capability=Write | Apply structured edits to a Jupyter notebook ...
- notes_remember | capability=Read | Persist a durable note ...
- verify | capability=Read | Run the project's app and observe behavior to verify a change.
- webfetch | capability=Web | Fetch a URL and return its content with redactions applied.
- websearch | capability=Web | Search the web via the configured backend.
</tools_index>
```

The model's job is to apply a non-trivial change to a Jupyter notebook.
It scans the index, sees `notebook_edit`, but its parameter schema is
not in `request.tools` yet, so trying to call it would be rejected by
the provider's tool-name validation. Instead the model emits:

```json
{"name": "load_tool_schema", "arguments": {"name": "notebook_edit"}}
```

`handle_load_tool_schema_call` looks `notebook_edit` up in
`all_tool_specs`, confirms it is not already in core, pushes the name
onto `loaded_tool_schemas`, and returns `{ok:true, status:"attached",
position:0}`. The next LLM round rebuilds the request's `tools` array
via `request_tool_specs`: the `for name in loaded_tool_schemas` tail
appends `notebook_edit`'s spec. The model now has the full schema in the
request and issues `notebook_edit(...)`. Accounting for that second
round: ~7.5KB core + 2KB index + ~1.5KB `notebook_edit` schema ≈ 11KB on
the wire instead of the 30-80KB an eager bundle would have been.

The same flow for a skill body. The session starts with two active
skills (say `verify` and `review`), each rendered as a
`<skill ... body="omitted">` metadata stub of perhaps 600 bytes instead
of a 5-10KB inlined body. Each stub carries the literal instruction
`Skill body omitted; call load_skill with name "verify" to load the
full instructions.`. The user asks "verify this PR"; the model calls
`load_skill({"name":"verify"})`. `SkillCatalog::load` reads SKILL.md
off disk on the first call and caches the parsed body. The tool result
lands as a single `FunctionCallOutput` item in the conversation —
*conversation*, not *system prompt*, so it lives in the message history
until compaction collapses it, never in the request's instructions
block. A second `load_skill("verify")` in the same session resolves
from the in-memory cache; no disk read.

## Edge cases and limits

**Lifetime of an attached schema.** Once a name is pushed onto
`loaded_tool_schemas`, it stays for the rest of the session. There is
no TTL, no eviction, and no per-turn reset — the agent's
`Arc<Mutex<Vec<String>>>` is initialised once at construction
(crates/squeezy-agent/src/lib.rs:1594) and never drained. The cross-turn
contract is asserted in `loaded_tool_schemas_persist_across_turns`
(crates/squeezy-agent/tests/tool_loop.rs:851-916): a tool loaded in
turn 1 appears in round 0 of turn 2 with its full schema intact. The
implication is monotonic growth: the request `tools` array only gets
*bigger* over a long session, which is correct (the model paid the
round-trip; making it pay again would be hostile), but it does mean
that a session that loads every discoverable schema converges to the
same bundle size as `lazy_schema_loading = false`. Session-start is the
cheap case; pathological long-tail loading is the upper bound, not the
steady state.

**MCP tools and the cache breakpoint.** Anthropic-style prompt caching
needs a stable prefix. The Anthropic provider marks the *last
first-party tool definition* with `cache_control: ephemeral`; MCP tools
ride past the breakpoint at the tail. The cache-policy module computes
the breakpoint index from the last *stable* (non-`mcp__`-prefixed) tool:

```rust
// crates/squeezy-llm/src/cache_policy.rs:155-190
/// Tool-name prefix the agent reserves for dynamically advertised MCP
/// tools. The tool registry pushes any tool whose name starts with this
/// to the *end* of the advertised list, so the cache breakpoint must
/// land before them — otherwise an MCP `tools/list` refresh that
/// reorders or replaces dynamic tools would invalidate the cached
/// prompt prefix on every turn.
pub(crate) const DYNAMIC_TOOL_NAME_PREFIX: &str = "mcp__";

pub(crate) fn last_stable_tool_index<'a, I>(names: I) -> Option<usize>
where
    I: IntoIterator<Item = &'a str>,
    I::IntoIter: DoubleEndedIterator + ExactSizeIterator,
{
    let iter = names.into_iter();
    let len = iter.len();
    if len == 0 { return None; }
    let stable = iter.enumerate().rev()
        .find_map(|(idx, name)| (!name.starts_with(DYNAMIC_TOOL_NAME_PREFIX)).then_some(idx));
    Some(stable.unwrap_or(len - 1))
}
```

The interaction with lazy loading: when the model calls
`load_tool_schema("mcp__notion__create_page")`, the dynamic name is
appended to `loaded_tool_schemas` and lands after the first-party tools
in the request `tools` array. The cache breakpoint stays on the last
*stable* tool, so the cached prefix still covers the byte-stable
first-party block. Without this rule, every MCP `tools/list` refresh
would invalidate the prefix cache on every turn.

**Misconfiguration silent-skip.** Names in `[tools].core` or
`[tools].discoverable` that do not match a known tool warn once at
session start (`warn_unknown_tool_schema_names`,
crates/squeezy-agent/src/lib.rs:11742-11766) and are otherwise ignored.
This protects the hot path (`push_tool_spec_by_name`) from doing a
linear scan and a `tracing::warn!` per round; a typo costs a one-shot
startup warning, not per-request overhead.

**Skill inline escape hatch.** `[skills] inline = true` reverts to
splicing the full body into the system prompt. The flag is read once at
catalog construction and stored as a `bool` on the catalog so every
render-time decision is a single comparison
(crates/squeezy-skills/src/lib.rs:384-390). The corresponding core
config (crates/squeezy-core/src/lib.rs:6175-6179) documents it as the
legacy mode. Operators with strong reasons to pay the per-turn
skill-body cost (a model with no tool-calling support, or a skill whose
triggers fire on every turn and the round-trip is wasted) can flip it.

**Is the index entry enough to decide?** Each row is `- {name} |
capability={cap} | {first-line-of-description}`. The capability label
(`Read`, `Write`, `Web`, `Exec`, `Permission`) tells the model which
session-mode gate the tool sits behind; the first description line is
the same string a human operator sees in help output. Squeezy's
first-party first lines are hand-tuned to be self-sufficient pitches
(`notebook_edit`, `webfetch`, `verify`). MCP tools inherit whatever
description their server published, which is usually adequate for
routing but can be terse — the cost of a one-line catalog miss is a
single `load_tool_schema` round-trip and ~1KB of schema, far smaller
than the cost of eager-advertising every MCP tool.

## Cost intuition

Tool schemas are a *prefix* on every request. With prompt caching they
are billed at write rates on the first request that establishes the
prefix and at read rates on every subsequent request — provided the
prefix bytes are unchanged. Two facts about lazy loading make that
prefix cheap and stable:

1. **Smaller prefix.** A fresh session ships roughly 6-8KB of tool
   schemas (eighteen core tools plus three control tools) plus a 1-2KB
   index, against 30-80KB for an eager bundle. The session-start
   compression is roughly 70-90%.
2. **More stable prefix.** The index is alphabetically sorted at render
   time (not insertion-order like `request_tool_specs`), and the core
   set is byte-stable across sessions. MCP tools are pushed past the
   cache breakpoint, so their churn does not invalidate the cached
   prefix. As long as `lazy_schema_loading` is on and no
   `load_tool_schema` call has fired yet, every round in the turn
   re-hits the cached prefix.

The mechanism is also a *graceful upper bound*. The worst case — a
session that calls `load_tool_schema` for every advertised name —
degenerates to the same byte cost as `lazy_schema_loading = false`,
plus a constant ~25 tool-result round trips. The expected case is the
opposite: most sessions touch a handful of tools, and the bundle stays
near its 6-8KB floor for the entire session.

Skill bodies sit in the same shape one layer up. An active-skills
bundle of metadata stubs costs ~500-800 bytes per skill, against 5-10KB
per inlined body. A model that needs the body pays for it exactly once
as a tool-result item in the conversation history (where it can be
folded into a compaction summary later); a model that does not need it
pays nothing beyond the stub. The arithmetic is the same as for tool
schemas: lazy-loading turns an unconditional cost into a per-use cost,
and pairs the saving with the byte-stable prefix the caching layer
relies on.
