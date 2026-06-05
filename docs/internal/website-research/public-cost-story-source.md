# Public Cost Story Source Map

The public website should use `docs/internal/cost-saving/` as the source of
truth for Squeezy's cost-saving story, but the marketing pages should translate
those chapters into outcomes rather than implementation terms.

## Public Buckets

| Public copy | Internal source |
|---|---|
| Reuse stable prompt context when providers support it | `01-provider-prompt-caching.md` |
| Keep long sessions from replaying everything | `02-conversation-compaction.md` |
| Avoid paying twice for repeated output | `03-tool-output-dedup-and-receipts.md` |
| Send the useful part of command output | `04-structured-tool-output.md` |
| Read targeted code context before broad source context | `05-ast-code-retrieval.md`, `13-graph-retrieval-in-practice.md` |
| Load tools and skills only when needed | `06-lazy-schema-loading.md` |
| Resume without replaying the whole past | `07-session-persistence-and-memory.md` |
| Keep exploration off the main thread | `08-sub-agent-isolation.md` |
| Control how much the agent says and returns | `09-verbosity-controls.md` |
| Show where tokens go | `10-token-accounting.md` |
| Use cheaper routes for simple turns | `11-cheap-model-fast-path.md` |

## Guardrails

- Marketing pages should not lead with internal tool names or parser/runtime
  implementation details.
- The homepage should treat local code understanding as one cost lever among
  several, not the whole product story.
- Benchmark claims must state the comparison target and task shape.
- Do not imply guaranteed savings on every task.
