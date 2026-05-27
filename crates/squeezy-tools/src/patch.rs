//! `plan_patch` and `apply_patch` implementations, plus the binding between
//! them.
//!
//! # Plan ↔ apply binding
//!
//! `plan_patch` walks the semantic graph from an objective + optional
//! symbol/path/query and returns a `plan_id` together with the set of paths
//! that make up the **plan neighborhood** (direct hits, references, callers,
//! callees, tests, configs, codeowners). The neighborhood is registered in
//! [`ToolRegistry::patch_plans`] under the `plan_id` for [`PATCH_PLAN_TTL`].
//!
//! `apply_patch` consults that registry: when the caller passes `plan_id`,
//! every touched path must lie inside the registered neighborhood, otherwise
//! the call returns [`ToolStatus::Stale`] without mutating disk. The caller
//! can opt out per-invocation with `confirm_outside_plan=true`, which is
//! reserved for "the plan was right but I learned something new" cases. The
//! enforcement site is the F84 block in `execute_apply_patch_blocking`; see
//! `lookup_patch_plan` for the cleanup-on-read semantics.
//!
//! This binding is intentional and is the reason Squeezy can refuse to drift
//! outside the plan even when the model emits a wider edit set; see
//! `audits/codex-comparison-2026-05-25/vcs.md` finding `G-vcs-F4` for the
//! rationale and the comparison with Codex's no-planning model. The binding
//! is graph-anchored rather than path-prefix based on purpose: a refactor's
//! neighborhood follows call/reference edges, not directory structure.

use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use globset::{Glob, GlobSetBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_vcs::{DiffMode, DiffOptions};

use crate::{
    MAX_GRAPH_MAX_RESULTS, StagedApply, ToolCall, ToolCostHint, ToolRegistry, ToolResult,
    ToolStatus, diff_file_json, diff_mode_str, graph_tools::graph_payload,
    graph_tools::reference_json, graph_tools::resolve_definition_candidates,
    graph_tools::symbol_json, graph_tools::symbol_summary_json, is_secret_path, make_result,
    safety, sha256_hex, tool_arg_error, tool_error, unix_timestamp_millis,
};

pub(crate) const DEFAULT_PATCH_MAX_SYMBOLS: usize = 8;
pub(crate) const DEFAULT_PATCH_MAX_RELATED: usize = 12;
pub(crate) const MAX_PATCH_BLOCKS: usize = 32;
pub(crate) const PATCH_SNIPPET_MAX_CHARS: usize = 2_000;

pub(crate) const PATCH_PLAN_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub(crate) struct PatchPlan {
    pub(crate) neighborhood: BTreeSet<String>,
    pub(crate) expires_at_ms: u128,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffContextArgs {
    pub(crate) mode: Option<DiffMode>,
    include_patch: Option<bool>,
    max_files: Option<usize>,
    max_symbols_per_file: Option<usize>,
    max_references_per_symbol: Option<usize>,
    max_patch_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanPatchArgs {
    pub(crate) objective: String,
    query: Option<String>,
    symbol_id: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    candidate_paths: Option<Vec<String>>,
    max_symbols: Option<usize>,
    max_related: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyPatchArgs {
    #[serde(default)]
    pub(crate) patches: Vec<SearchReplacePatch>,
    #[serde(default)]
    operations: Vec<ApplyPatchOperation>,
    impact_paths: Option<Vec<String>>,
    plan_id: Option<String>,
    dry_run: Option<bool>,
    #[serde(default)]
    confirm_outside_plan: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchReplacePatch {
    pub(crate) path: String,
    search: String,
    replace: String,
    expected_sha256: Option<String>,
    allow_multiple: Option<bool>,
    #[serde(default)]
    fallback: Option<SearchReplaceFallback>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchReplaceFallback {
    UnifiedDiff,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ApplyPatchOperation {
    SearchReplace {
        path: String,
        search: String,
        replace: String,
        expected_sha256: Option<String>,
        #[serde(default)]
        allow_multiple: Option<bool>,
        #[serde(default)]
        fallback: Option<SearchReplaceFallback>,
    },
    CreateFile {
        path: String,
        contents: String,
        #[serde(default)]
        expected_absent: Option<bool>,
    },
    DeleteFile {
        path: String,
        expected_sha256: Option<String>,
    },
    MoveFile {
        from: String,
        to: String,
        expected_sha256: Option<String>,
        #[serde(default)]
        post_replace: Option<PostMoveReplace>,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostMoveReplace {
    pub(crate) search: String,
    pub(crate) replace: String,
    #[serde(default)]
    pub(crate) allow_multiple: Option<bool>,
}

impl ToolRegistry {
    pub(crate) async fn execute_diff_context(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<DiffContextArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let registry = self.clone();
        let call = call.clone();
        tokio::task::spawn_blocking(move || registry.execute_diff_context_blocking(&call, args))
            .await
            .unwrap_or_else(|err| {
                make_result(
                    &ToolCall {
                        call_id: String::new(),
                        name: "diff_context".to_string(),
                        arguments: Value::Null,
                    },
                    ToolStatus::Error,
                    json!({ "error": format!("diff_context join failed: {err}") }),
                    ToolCostHint::default(),
                    None,
                )
            })
    }

    fn execute_diff_context_blocking(&self, call: &ToolCall, args: DiffContextArgs) -> ToolResult {
        let max_patch_bytes = args.max_patch_bytes.unwrap_or(1_000_000).min(5_000_000);
        let snapshot = self.diff_snapshot(
            args.mode.unwrap_or_default(),
            DiffOptions {
                include_patch: args.include_patch.unwrap_or(false),
                max_patch_bytes,
            },
        );
        let max_files = args.max_files.unwrap_or(100).min(500);
        let max_symbols_per_file = args.max_symbols_per_file.unwrap_or(12).min(100);
        let max_references = args.max_references_per_symbol.unwrap_or(8).min(50);
        let graph_context =
            self.graph_context_for_snapshot(&snapshot, max_symbols_per_file, max_references);
        let files = snapshot
            .files
            .iter()
            .take(max_files)
            .map(diff_file_json)
            .collect::<Vec<_>>();
        let truncated = snapshot.truncated || snapshot.files.len() > max_files;

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "vcs": snapshot.vcs,
                "mode": diff_mode_str(snapshot.mode),
                "summary": snapshot.summary,
                "files": files,
                "graph": graph_context,
                "truncated": truncated,
                "errors": snapshot.errors,
            }),
            ToolCostHint {
                matches_returned: snapshot.files.len().min(max_files) as u64,
                truncated,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    pub(crate) async fn execute_plan_patch(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<PlanPatchArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let registry = self.clone();
        let call_for_error = call.clone();
        let call_for_blocking = call.clone();
        tokio::task::spawn_blocking(move || {
            registry.execute_plan_patch_blocking(&call_for_blocking, args)
        })
        .await
        .unwrap_or_else(|err| {
            make_result(
                &call_for_error,
                ToolStatus::Error,
                json!({ "error": format!("plan_patch join failed: {err}") }),
                ToolCostHint::default(),
                None,
            )
        })
    }

    fn execute_plan_patch_blocking(&self, call: &ToolCall, args: PlanPatchArgs) -> ToolResult {
        let max_symbols = args
            .max_symbols
            .unwrap_or(DEFAULT_PATCH_MAX_SYMBOLS)
            .clamp(1, MAX_GRAPH_MAX_RESULTS);
        let max_related = args
            .max_related
            .unwrap_or(DEFAULT_PATCH_MAX_RELATED)
            .clamp(1, MAX_GRAPH_MAX_RESULTS);
        let candidate_paths = normalized_path_set(args.candidate_paths.as_deref().unwrap_or(&[]));
        let mut graph = match self.graph.lock() {
            Ok(graph) => graph,
            Err(_) => {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({"error": "semantic graph lock poisoned"}),
                    ToolCostHint::default(),
                    None,
                );
            }
        };
        let Some(manager) = graph.as_mut() else {
            let locality = patch_locality_json(&candidate_paths, &BTreeSet::new());
            let plan_id = patch_plan_id(&call.arguments, &candidate_paths);
            let next_action = if candidate_paths.is_empty() {
                json!({
                    "tool": "decl_search",
                    "arguments_template": {
                        "query": args.query.as_deref().unwrap_or("<symbol or text>")
                    },
                    "reason": "semantic graph is unavailable; widen the search with decl_search or grep before patching",
                    "fallback_tools": ["decl_search", "grep"]
                })
            } else {
                patch_next_action(&candidate_paths, plan_id.clone())
            };
            return make_result(
                call,
                ToolStatus::Success,
                json!({
                    "tool": "plan_patch",
                    "status": "graph_unavailable",
                    "graph_available": false,
                    "reason": "semantic graph is unavailable for this workspace",
                    "objective": args.objective,
                    "patch_format": "search_replace",
                    "plan_id": plan_id,
                    "impact": {
                        "neighborhood_paths": candidate_paths.iter().cloned().collect::<Vec<_>>(),
                        "fallback": {
                            "status": "graph_unavailable",
                            "suggested_tools": [
                                {"tool": "grep", "arguments_template": {"pattern": args.query.as_deref().unwrap_or("<query>"), "output_mode": "files_with_matches"}},
                                {"tool": "read_file", "arguments_template": {"path": "<candidate-path>"}}
                            ]
                        }
                    },
                    "locality": locality,
                    "next_action": next_action,
                }),
                ToolCostHint::default(),
                None,
            );
        };
        let refresh = match manager.refresh_before_query() {
            Ok(report) => report,
            Err(err) => return tool_error(call, err),
        };
        let graph = manager.graph();
        let mut symbols = resolve_definition_candidates(
            graph,
            args.symbol_id.as_deref(),
            args.query.as_deref(),
            args.kind.as_deref(),
            args.path.as_deref(),
            None,
        );
        let symbols_truncated = symbols.len() > max_symbols;
        symbols.truncate(max_symbols);

        let mut direct_paths = BTreeSet::new();
        let mut reference_paths = BTreeSet::new();
        let mut caller_paths = BTreeSet::new();
        let mut callee_paths = BTreeSet::new();
        let mut references = Vec::new();
        let mut callers = Vec::new();
        let mut callees = Vec::new();

        for symbol in &symbols {
            direct_paths.insert(symbol.file_id.0.clone());
            for hit in graph
                .references_to_symbol(&symbol.id)
                .into_iter()
                .take(max_related)
            {
                reference_paths.insert(hit.reference.file_id.0.clone());
                references.push(reference_json(hit));
            }
            for hit in graph.callers(&symbol.id).into_iter().take(max_related) {
                if let Some(caller) = hit.caller {
                    caller_paths.insert(caller.file_id.0.clone());
                    callers.push(symbol_summary_json(&caller));
                }
            }
            for hit in graph.callees(&symbol.id).into_iter().take(max_related) {
                if let Some(callee) = hit.callee {
                    callee_paths.insert(callee.file_id.0.clone());
                    callees.push(symbol_summary_json(&callee));
                }
            }
        }

        let graph_paths = graph
            .files
            .keys()
            .map(|file_id| file_id.0.as_str())
            .collect::<Vec<_>>();
        let mut neighborhood = BTreeSet::new();
        neighborhood.extend(direct_paths.iter().cloned());
        neighborhood.extend(reference_paths.iter().cloned());
        neighborhood.extend(caller_paths.iter().cloned());
        neighborhood.extend(callee_paths.iter().cloned());
        let test_paths = test_candidate_paths(&graph_paths, &neighborhood);
        let config_paths = config_candidate_paths(&self.root, &neighborhood);
        let owner_paths = owner_candidate_paths(&self.root);
        neighborhood.extend(test_paths.iter().cloned());
        neighborhood.extend(config_paths.iter().cloned());
        neighborhood.extend(owner_paths.iter().cloned());

        let locality = patch_locality_json(&candidate_paths, &neighborhood);
        neighborhood.extend(candidate_paths.iter().cloned());

        let plan_id = patch_plan_id(&call.arguments, &neighborhood);
        self.register_patch_plan(&plan_id, &neighborhood);
        let owners = codeowner_matches(&self.root, &neighborhood);
        let mut payload = graph_payload("plan_patch", manager, &refresh);
        payload.insert("objective".to_string(), json!(args.objective));
        payload.insert("query".to_string(), json!(args.query));
        payload.insert("symbol_id".to_string(), json!(args.symbol_id));
        payload.insert("patch_format".to_string(), json!("search_replace"));
        payload.insert("plan_id".to_string(), json!(plan_id.clone()));
        payload.insert(
            "symbols".to_string(),
            json!(
                symbols
                    .iter()
                    .map(|symbol| symbol_json(graph, symbol))
                    .collect::<Vec<_>>()
            ),
        );
        payload.insert(
            "impact".to_string(),
            json!({
                "direct_paths": direct_paths.iter().cloned().collect::<Vec<_>>(),
                "reference_paths": reference_paths.iter().cloned().collect::<Vec<_>>(),
                "caller_paths": caller_paths.iter().cloned().collect::<Vec<_>>(),
                "callee_paths": callee_paths.iter().cloned().collect::<Vec<_>>(),
                "test_paths": test_paths.iter().cloned().collect::<Vec<_>>(),
                "config_paths": config_paths.iter().cloned().collect::<Vec<_>>(),
                "owner_paths": owner_paths.iter().cloned().collect::<Vec<_>>(),
                "neighborhood_paths": neighborhood.iter().cloned().collect::<Vec<_>>(),
                "references": references,
                "callers": callers,
                "callees": callees,
                "owners": owners,
            }),
        );
        payload.insert("locality".to_string(), locality);
        let next_action = if symbols.is_empty() && neighborhood.is_empty() {
            json!({
                "tool": "decl_search",
                "arguments_template": {
                    "query": args.query.as_deref().unwrap_or("<symbol or text>")
                },
                "reason": "plan_patch found no graph evidence; widen the search with decl_search or fall back to grep before patching",
                "fallback_tools": ["decl_search", "grep"]
            })
        } else {
            patch_next_action(&neighborhood, plan_id)
        };
        payload.insert("next_action".to_string(), next_action);

        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: symbols.len() as u64,
                truncated: symbols_truncated,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    pub(crate) async fn execute_apply_patch(&self, call: &ToolCall, group_id: &str) -> ToolResult {
        let args = match serde_json::from_value::<ApplyPatchArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        if !args.patches.is_empty() && !args.operations.is_empty() {
            return tool_error(
                call,
                "apply_patch accepts either `patches` (legacy) or `operations`, not both",
            );
        }
        let raw_ops: Vec<ApplyPatchOperation> = if !args.operations.is_empty() {
            args.operations.clone()
        } else {
            args.patches
                .iter()
                .cloned()
                .map(|patch| ApplyPatchOperation::SearchReplace {
                    path: patch.path,
                    search: patch.search,
                    replace: patch.replace,
                    expected_sha256: patch.expected_sha256,
                    allow_multiple: patch.allow_multiple,
                    fallback: patch.fallback,
                })
                .collect()
        };
        if raw_ops.is_empty() {
            return make_result(
                call,
                ToolStatus::Error,
                json!({ "error": "apply_patch requires at least one patch block" }),
                ToolCostHint::default(),
                None,
            );
        }
        if raw_ops.len() > MAX_PATCH_BLOCKS {
            return make_result(
                call,
                ToolStatus::Error,
                json!({
                    "error": format!("apply_patch accepts at most {MAX_PATCH_BLOCKS} patch blocks")
                }),
                ToolCostHint::default(),
                None,
            );
        }

        let dry_run = args.dry_run.unwrap_or(false);
        let impact_paths = normalized_path_set(args.impact_paths.as_deref().unwrap_or(&[]));
        // Collect every workspace-relative path each op touches (for locality,
        // plan-binding, and secret-path checks).
        let touched_paths: Vec<String> = raw_ops
            .iter()
            .flat_map(|op| match op {
                ApplyPatchOperation::SearchReplace { path, .. }
                | ApplyPatchOperation::CreateFile { path, .. }
                | ApplyPatchOperation::DeleteFile { path, .. } => vec![path.clone()],
                ApplyPatchOperation::MoveFile { from, to, .. } => vec![from.clone(), to.clone()],
            })
            .collect();
        let patch_paths = normalized_path_set(&touched_paths);
        let locality = patch_locality_json(&patch_paths, &impact_paths);
        let warnings = patch_locality_warnings(&patch_paths, &impact_paths);

        // Plan-binding (F84): every touched path must intersect the plan's
        // neighborhood, unless the caller explicitly opts out.
        if let Some(plan_id) = args.plan_id.as_deref()
            && let Some(plan) = self.lookup_patch_plan(plan_id)
            && !args.confirm_outside_plan
        {
            let outside: Vec<String> = patch_paths
                .iter()
                .filter(|path| !plan.neighborhood.contains(*path))
                .cloned()
                .collect();
            if !outside.is_empty() {
                return make_result(
                    call,
                    ToolStatus::Stale,
                    json!({
                        "error": format!(
                            "patch escapes plan_id {plan_id} neighborhood; pass confirm_outside_plan=true to override"
                        ),
                        "plan_id": plan_id,
                        "outside_paths": outside,
                        "neighborhood": plan.neighborhood.iter().cloned().collect::<Vec<_>>(),
                    }),
                    ToolCostHint::default(),
                    None,
                );
            }
        }

        // Sandbox + secret + protected-metadata block — applied per-op so the
        // legacy patches[] and new operations[] shapes share one safety floor.
        for op in &raw_ops {
            let op_paths: Vec<String> = match op {
                ApplyPatchOperation::SearchReplace { path, .. }
                | ApplyPatchOperation::CreateFile { path, .. }
                | ApplyPatchOperation::DeleteFile { path, .. } => vec![path.clone()],
                ApplyPatchOperation::MoveFile { from, to, .. } => {
                    vec![from.clone(), to.clone()]
                }
            };
            for rel in &op_paths {
                if let Err(err) = safety::assess_write_path(rel, &self.root, &self.shell_sandbox) {
                    return make_result(
                        call,
                        ToolStatus::Denied,
                        json!({
                            "error": err.message(),
                            "path": rel,
                            "reason": err.code(),
                            "permission_denied": true,
                            "policy_denied": true,
                        }),
                        ToolCostHint::default(),
                        None,
                    );
                }
                let absolute = self.root.join(rel);
                if is_secret_path(Path::new(rel))
                    || safety::path_targets_protected_metadata(
                        &absolute,
                        &self.root,
                        &self.shell_sandbox,
                    )
                    .is_some()
                {
                    return make_result(
                        call,
                        ToolStatus::Denied,
                        json!({
                            "error": "refusing to patch a likely secret or protected metadata file",
                            "path": rel,
                            "permission_denied": true,
                            "policy_denied": true,
                        }),
                        ToolCostHint::default(),
                        None,
                    );
                }
            }
        }

        // Capture the checkpoint snapshot before validation. The
        // `unified_diff` fallback (F89) shells out to `git apply` during
        // validation, which mutates the worktree directly — without an
        // up-front snapshot the post-mutation tree would be both the
        // "before" and "after" and no checkpoint would record the change.
        let checkpoint_before = if dry_run {
            None
        } else {
            match self.track_checkpoint_tree() {
                Ok(snapshot) => snapshot,
                Err(err) => return tool_error(call, err),
            }
        };

        // Stage every op in memory. We materialise the final intended state for
        // each file path so the write phase can be a simple "write final
        // contents" loop, and so dry-run can preview without touching disk.
        let mut staged = StagedApply::default();
        let mut preview_ops = Vec::new();
        for (index, op) in raw_ops.iter().enumerate() {
            match self.stage_apply_patch_op(call, index, op, &mut staged, &mut preview_ops) {
                Ok(()) => {}
                Err(result) => return result,
            }
        }

        let changed_files = staged.changed_files_json();
        let bytes_read = staged.bytes_read();
        let bytes_written = staged.bytes_written();

        if dry_run {
            let preview_delta = staged.delta_preview_json(false);
            let summary: Vec<Value> = staged
                .ops
                .iter()
                .map(|op| {
                    json!({
                        "path": op.primary_path(),
                        "status": "applied",
                        "exact": op.exact(),
                    })
                })
                .collect();
            let exact_overall = staged.ops.iter().all(|op| op.exact());
            let content = json!({
                "dry_run": true,
                "plan_id": args.plan_id,
                "patch_format": "search_replace",
                "operations": preview_ops,
                "files": changed_files,
                "locality": locality,
                "warnings": warnings,
                "applied_delta": {
                    "exact": exact_overall,
                    "operations": preview_delta,
                },
                "delta": summary,
            });
            return make_result(
                call,
                ToolStatus::Success,
                content,
                ToolCostHint {
                    bytes_read,
                    output_bytes: bytes_written,
                    ..ToolCostHint::default()
                },
                None,
            );
        }

        let mut applied_delta = Vec::with_capacity(staged.ops.len());
        let mut write_failure: Option<(String, String, usize)> = None;
        let mut written: BTreeSet<usize> = BTreeSet::new();
        for (idx, op) in staged.ops.iter().enumerate() {
            if write_failure.is_some() {
                applied_delta.push(op.delta_json_full("skipped", idx, op.exact(), None));
                continue;
            }
            match op.apply(&staged.files, &mut written) {
                Ok(()) => {
                    applied_delta.push(op.delta_json_full("applied", idx, op.exact(), None));
                }
                Err(err) => {
                    let message = err.to_string();
                    applied_delta.push(op.delta_json_full(
                        "failed",
                        idx,
                        op.exact(),
                        Some(&message),
                    ));
                    write_failure = Some((op.primary_path().to_string(), message, idx));
                }
            }
        }
        self.invalidate_diff_cache();
        let exact_delta = write_failure.is_none() && staged.ops.iter().all(|op| op.exact());
        let delta_summary = audit_delta_summary(&applied_delta);

        if let Some((failed_path, error, _idx)) = write_failure {
            let mut error_content = json!({
                "error": format!("failed to apply op at {failed_path}: {error}"),
                "failed_path": failed_path,
                "plan_id": args.plan_id,
                "patch_format": "search_replace",
                "operations": preview_ops,
                "files": changed_files,
                "locality": locality,
                "warnings": warnings,
                "applied_delta": {
                    "exact": exact_delta,
                    "operations": applied_delta,
                },
                "delta": delta_summary,
            });
            self.append_checkpoint_to_content(
                &mut error_content,
                checkpoint_before.as_ref(),
                call,
                group_id,
                ToolStatus::Error,
                Vec::new(),
            );
            return make_result(
                call,
                ToolStatus::Error,
                error_content,
                ToolCostHint {
                    bytes_read,
                    output_bytes: bytes_written,
                    ..ToolCostHint::default()
                },
                None,
            );
        }
        let mut content = json!({
            "dry_run": false,
            "plan_id": args.plan_id,
            "patch_format": "search_replace",
            "operations": preview_ops,
            "files": changed_files,
            "locality": locality,
            "warnings": warnings,
            "applied_delta": {
                "exact": exact_delta,
                "operations": applied_delta,
            },
            "delta": delta_summary,
        });
        self.append_checkpoint_to_content(
            &mut content,
            checkpoint_before.as_ref(),
            call,
            group_id,
            ToolStatus::Success,
            Vec::new(),
        );
        make_result(
            call,
            ToolStatus::Success,
            content,
            ToolCostHint {
                bytes_read,
                output_bytes: bytes_written,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    pub(crate) fn register_patch_plan(&self, plan_id: &str, neighborhood: &BTreeSet<String>) {
        let now = unix_timestamp_millis(SystemTime::now());
        let expires_at_ms = now.saturating_add(PATCH_PLAN_TTL.as_millis());
        let Ok(mut plans) = self.patch_plans.lock() else {
            return;
        };
        // Purge expired entries on insert to keep the map bounded.
        plans.retain(|_, plan| plan.expires_at_ms > now);
        plans.insert(
            plan_id.to_string(),
            PatchPlan {
                neighborhood: neighborhood.clone(),
                expires_at_ms,
            },
        );
    }

    pub(crate) fn lookup_patch_plan(&self, plan_id: &str) -> Option<PatchPlan> {
        let now = unix_timestamp_millis(SystemTime::now());
        let mut plans = self.patch_plans.lock().ok()?;
        plans.retain(|_, plan| plan.expires_at_ms > now);
        plans.get(plan_id).cloned()
    }
}

/// Build the audit-facing per-op summary array — `[{path, status, exact,
/// error?}]` — from the rich `applied_delta.operations` entries. Callers that
/// only want to know which paths landed exactly can read this without parsing
/// the full delta shape.
pub(crate) fn audit_delta_summary(entries: &[Value]) -> Vec<Value> {
    entries
        .iter()
        .map(|entry| {
            let mut summary = json!({
                "path": entry.get("path").cloned().unwrap_or(Value::Null),
                "status": entry.get("status").cloned().unwrap_or(Value::Null),
                "exact": entry
                    .get("exact")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
            });
            if let Some(error) = entry.get("error").and_then(|v| v.as_str())
                && let Some(obj) = summary.as_object_mut()
            {
                obj.insert("error".to_string(), json!(error));
            }
            summary
        })
        .collect()
}

pub(crate) fn normalized_path_set(paths: &[String]) -> BTreeSet<String> {
    paths
        .iter()
        .filter_map(|path| normalize_workspace_path_str(path))
        .collect()
}

fn normalize_workspace_path_str(path: &str) -> Option<String> {
    let normalized = path.trim().replace('\\', "/");
    if normalized.is_empty() {
        return None;
    }
    let normalized = normalized
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string();
    (!normalized.is_empty()).then_some(normalized)
}

fn path_in_neighborhood(path: &str, neighborhood: &BTreeSet<String>) -> bool {
    if neighborhood.is_empty() {
        return false;
    }
    neighborhood.iter().any(|candidate| {
        path == candidate
            || path
                .strip_prefix(candidate)
                .is_some_and(|suffix| suffix.starts_with('/'))
            || candidate
                .strip_prefix(path)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

pub(crate) fn patch_locality_json(
    patch_paths: &BTreeSet<String>,
    neighborhood: &BTreeSet<String>,
) -> Value {
    if neighborhood.is_empty() {
        return json!({
            "checked": false,
            "status": "unchecked",
            "reason": "no impact paths were supplied",
            "inside_paths": [],
            "outside_paths": patch_paths.iter().cloned().collect::<Vec<_>>(),
        });
    }
    let inside = patch_paths
        .iter()
        .filter(|path| path_in_neighborhood(path, neighborhood))
        .cloned()
        .collect::<Vec<_>>();
    let outside = patch_paths
        .iter()
        .filter(|path| !path_in_neighborhood(path, neighborhood))
        .cloned()
        .collect::<Vec<_>>();
    json!({
        "checked": true,
        "status": if outside.is_empty() { "inside" } else { "outside" },
        "inside_paths": inside,
        "outside_paths": outside,
        "neighborhood_paths": neighborhood.iter().cloned().collect::<Vec<_>>(),
        "warning": (!outside.is_empty()).then_some("patch touches paths outside the impacted graph neighborhood"),
    })
}

fn patch_locality_warnings(
    patch_paths: &BTreeSet<String>,
    neighborhood: &BTreeSet<String>,
) -> Vec<String> {
    if neighborhood.is_empty() {
        return Vec::new();
    }
    let outside = patch_paths
        .iter()
        .filter(|path| !path_in_neighborhood(path, neighborhood))
        .cloned()
        .collect::<Vec<_>>();
    if outside.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "patch touches paths outside the impacted graph neighborhood: {}",
            outside.join(", ")
        )]
    }
}

fn patch_plan_id(arguments: &Value, paths: &BTreeSet<String>) -> String {
    let payload = json!({
        "arguments": arguments,
        "paths": paths.iter().cloned().collect::<Vec<_>>(),
    });
    let digest = sha256_hex(payload.to_string().as_bytes());
    format!("patch-{}", &digest[..12])
}

fn patch_next_action(paths: &BTreeSet<String>, plan_id: String) -> Value {
    json!({
        "tool": "apply_patch",
        "arguments": {
            "plan_id": plan_id,
            "impact_paths": paths.iter().cloned().collect::<Vec<_>>(),
            "patches": [
                {
                    "path": "<workspace-relative-path>",
                    "search": "<exact current text>",
                    "replace": "<replacement text>",
                    "expected_sha256": "<sha256 from read_file or read_slice context>"
                }
            ]
        },
        "reason": "apply exact search-replace blocks after reviewing the impacted neighborhood"
    })
}

fn test_candidate_paths(graph_paths: &[&str], neighborhood: &BTreeSet<String>) -> BTreeSet<String> {
    graph_paths
        .iter()
        .filter(|path| {
            neighborhood.iter().any(|impacted| {
                same_crate_path(path, impacted)
                    && (path.contains("/tests/")
                        || path.ends_with("_tests.rs")
                        || path.ends_with("/tests.rs"))
            })
        })
        .map(|path| (*path).to_string())
        .collect()
}

fn same_crate_path(left: &str, right: &str) -> bool {
    let mut left_parts = left.split('/');
    let mut right_parts = right.split('/');
    match (
        left_parts.next(),
        left_parts.next(),
        right_parts.next(),
        right_parts.next(),
    ) {
        (Some("crates"), Some(left_crate), Some("crates"), Some(right_crate)) => {
            left_crate == right_crate
        }
        _ => !left.starts_with("crates/") && !right.starts_with("crates/"),
    }
}

fn config_candidate_paths(root: &Path, neighborhood: &BTreeSet<String>) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for candidate in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        if root.join(candidate).exists() {
            paths.insert(candidate.to_string());
        }
    }
    for path in neighborhood {
        let mut parts = path.split('/');
        if let (Some("crates"), Some(crate_dir)) = (parts.next(), parts.next()) {
            let manifest = format!("crates/{crate_dir}/Cargo.toml");
            if root.join(&manifest).exists() {
                paths.insert(manifest);
            }
        }
    }
    paths
}

fn owner_candidate_paths(root: &Path) -> BTreeSet<String> {
    [".github/CODEOWNERS", "CODEOWNERS"]
        .into_iter()
        .filter(|path| root.join(path).exists())
        .map(str::to_string)
        .collect()
}

fn codeowner_matches(root: &Path, paths: &BTreeSet<String>) -> Vec<Value> {
    [".github/CODEOWNERS", "CODEOWNERS"]
        .into_iter()
        .find_map(|path| {
            fs::read_to_string(root.join(path))
                .ok()
                .map(|content| (path, content))
        })
        .map(|(owner_path, content)| {
            content
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        return None;
                    }
                    let mut parts = line.split_whitespace();
                    let pattern = parts.next()?;
                    let owners = parts.map(str::to_string).collect::<Vec<_>>();
                    if owners.is_empty()
                        || !paths
                            .iter()
                            .any(|path| codeowner_pattern_matches(pattern, path))
                    {
                        return None;
                    }
                    Some(json!({
                        "path": owner_path,
                        "pattern": pattern,
                        "owners": owners,
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn codeowner_pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    let anchored = pattern.starts_with('/');
    let bare = pattern.trim_start_matches('/');
    let bare = bare.trim_end_matches('/');
    if bare.is_empty() {
        return false;
    }
    let has_glob_meta = bare.contains(['*', '?', '[']);
    if !has_glob_meta {
        if anchored {
            return path == bare || path.starts_with(&format!("{bare}/"));
        }
        if path == bare {
            return true;
        }
        for (idx, _) in path.match_indices(bare) {
            let before_ok = idx == 0 || path.as_bytes().get(idx - 1) == Some(&b'/');
            let after = &path[idx + bare.len()..];
            let after_ok = after.is_empty() || after.starts_with('/');
            if before_ok && after_ok {
                return true;
            }
        }
        return false;
    }
    let mut builder = GlobSetBuilder::new();
    let primary = if anchored {
        bare.to_string()
    } else {
        format!("**/{bare}")
    };
    let Ok(primary_glob) = Glob::new(&primary) else {
        return false;
    };
    builder.add(primary_glob);
    if !anchored
        && !bare.contains('/')
        && let Ok(extra) = Glob::new(bare)
    {
        builder.add(extra);
    }
    if !bare.ends_with("/**") {
        let trailing = if anchored {
            format!("{bare}/**")
        } else {
            format!("**/{bare}/**")
        };
        if let Ok(glob) = Glob::new(&trailing) {
            builder.add(glob);
        }
    }
    builder
        .build()
        .map(|set| set.is_match(path))
        .unwrap_or(false)
}
