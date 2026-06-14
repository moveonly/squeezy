use serde::Deserialize;
use serde_json::json;
use squeezy_vcs::{RollbackMode, RollbackTarget};

use crate::{
    CHECKPOINTS_DISABLED_MESSAGE, ToolCall, ToolCostHint, ToolRegistry, ToolResult, ToolStatus,
    checkpoints_disabled_result, make_result, safety, tool_arg_error, tool_error,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointListArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointDoctorArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointUndoArgs {
    mode: Option<RollbackMode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointShowArgs {
    pub(crate) checkpoint_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointRevertArgs {
    pub(crate) group_id: Option<String>,
    pub(crate) checkpoint_id: Option<String>,
    mode: Option<RollbackMode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointRestoreFileArgs {
    pub(crate) checkpoint_id: String,
    pub(crate) path: String,
    mode: Option<RollbackMode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointCheckArgs {}

impl ToolRegistry {
    pub(crate) async fn execute_checkpoint_list(&self, call: &ToolCall) -> ToolResult {
        if let Err(err) = serde_json::from_value::<CheckpointListArgs>(call.arguments.clone()) {
            return tool_arg_error(call, err);
        }
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return make_result(
                call,
                ToolStatus::Success,
                json!({
                    "enabled": false,
                    "checkpoints": [],
                    "journal_warnings": 0,
                    "message": CHECKPOINTS_DISABLED_MESSAGE,
                }),
                ToolCostHint::default(),
                None,
            );
        };
        match checkpoints.read_journal() {
            Ok(journal) => {
                let mut checkpoints = journal.checkpoints;
                checkpoints.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
                make_result(
                    call,
                    ToolStatus::Success,
                    json!({
                        "checkpoints": checkpoints,
                        "journal_warnings": journal.journal_warnings,
                    }),
                    ToolCostHint {
                        matches_returned: checkpoints.len() as u64,
                        ..ToolCostHint::default()
                    },
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    pub(crate) async fn execute_checkpoint_show(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointShowArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return checkpoints_disabled_result(call);
        };
        match checkpoints.show_checkpoint(&args.checkpoint_id) {
            Ok(Some(checkpoint)) => make_result(
                call,
                ToolStatus::Success,
                json!({ "checkpoint": checkpoint }),
                ToolCostHint::default(),
                None,
            ),
            Ok(None) => make_result(
                call,
                ToolStatus::Stale,
                json!({
                    "error": "checkpoint not found",
                    "checkpoint_id": args.checkpoint_id,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(err) => tool_error(call, err),
        }
    }

    pub(crate) async fn execute_checkpoint_doctor(&self, call: &ToolCall) -> ToolResult {
        if let Err(err) = serde_json::from_value::<CheckpointDoctorArgs>(call.arguments.clone()) {
            return tool_arg_error(call, err);
        }
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return checkpoints_disabled_result(call);
        };
        match checkpoints.doctor() {
            Ok(report) => {
                let ok = report.protected_ref_roundtrip
                    && report.checkpoints_dir_writable
                    && report.warnings.is_empty();
                make_result(
                    call,
                    ToolStatus::Success,
                    json!({ "ok": ok, "doctor": report }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    pub(crate) async fn execute_checkpoint_undo(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointUndoArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return checkpoints_disabled_result(call);
        };
        let target = RollbackTarget::Latest;
        if let Some(result) = self.assess_rollback_paths(call, target) {
            return result;
        }
        match checkpoints.rollback(target, args.mode.unwrap_or_default()) {
            Ok(result) => {
                self.invalidate_diff_cache();
                self.record_restored_files_into_graph(&result.restored_files);
                // `rollback` returns `skipped && !applied` with no
                // conflicts when the journal had nothing to select. That
                // is the clean-tree happy path: no checkpoints exist to undo.
                let nothing_to_undo =
                    result.skipped && !result.applied && result.conflicts.is_empty();
                if nothing_to_undo {
                    return make_result(
                        call,
                        ToolStatus::Success,
                        json!({
                            "rollback": result,
                            "message": "nothing to undo",
                        }),
                        ToolCostHint::default(),
                        None,
                    );
                }
                let message = rollback_message(&result);
                make_result(
                    call,
                    if result.conflicts.is_empty() && !result.skipped && result.applied {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Stale
                    },
                    json!({
                        "rollback": result,
                        "message": message,
                    }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    pub(crate) async fn execute_checkpoint_revert(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointRevertArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let target = match (args.group_id.as_deref(), args.checkpoint_id.as_deref()) {
            (Some(group_id), None) => RollbackTarget::Group(group_id),
            (None, Some(checkpoint_id)) => RollbackTarget::Checkpoint(checkpoint_id),
            _ => {
                return tool_error(
                    call,
                    "provide exactly one of group_id or checkpoint_id for checkpoint_revert",
                );
            }
        };
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return checkpoints_disabled_result(call);
        };
        if let Some(result) = self.assess_rollback_paths(call, target) {
            return result;
        }
        match checkpoints.rollback(target, args.mode.unwrap_or_default()) {
            Ok(result) => {
                self.invalidate_diff_cache();
                self.record_restored_files_into_graph(&result.restored_files);
                let nothing_to_revert =
                    result.skipped && !result.applied && result.conflicts.is_empty();
                if nothing_to_revert {
                    return make_result(
                        call,
                        ToolStatus::Success,
                        json!({
                            "rollback": result,
                            "message": "nothing to revert",
                        }),
                        ToolCostHint::default(),
                        None,
                    );
                }
                let message = rollback_message(&result);
                make_result(
                    call,
                    if result.conflicts.is_empty() && !result.skipped && result.applied {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Stale
                    },
                    json!({
                        "rollback": result,
                        "message": message,
                    }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    pub(crate) async fn execute_checkpoint_restore_file(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointRestoreFileArgs>(call.arguments.clone())
        {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return checkpoints_disabled_result(call);
        };
        let paths = match checkpoints.restore_checkpoint_file_paths(&args.checkpoint_id, &args.path)
        {
            Ok(paths) if paths.is_empty() => vec![args.path.clone()],
            Ok(paths) => paths,
            Err(err) => return tool_error(call, err),
        };
        for path in paths {
            if let Err(err) = safety::assess_write_path(&path, &self.root, &self.shell_sandbox) {
                return make_result(
                    call,
                    ToolStatus::Denied,
                    json!({
                        "error": err.message(),
                        "path": path,
                        "reason": err.code(),
                        "permission_denied": true,
                        "policy_denied": true,
                    }),
                    ToolCostHint::default(),
                    None,
                );
            }
        }
        match checkpoints.restore_checkpoint_file(
            &args.checkpoint_id,
            &args.path,
            args.mode.unwrap_or_default(),
        ) {
            Ok(result) => {
                self.invalidate_diff_cache();
                self.record_restored_files_into_graph(&result.restored_files);
                let message = rollback_message(&result);
                make_result(
                    call,
                    if result.conflicts.is_empty() && !result.skipped && result.applied {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Stale
                    },
                    json!({
                        "rollback": result,
                        "message": message,
                    }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    /// Feed checkpoint-restored files (workspace-relative paths) into the
    /// semantic graph's pending-changed set so a later query reparses them.
    /// Resolves each against the workspace root; the graph canonicalizes
    /// during refresh. No-op when nothing was restored.
    fn record_restored_files_into_graph(&self, restored_files: &[String]) {
        if restored_files.is_empty() {
            return;
        }
        let abs_paths = restored_files
            .iter()
            .map(|rel| self.root.join(rel))
            .collect::<Vec<_>>();
        self.record_graph_changed_paths(abs_paths);
    }

    pub(crate) async fn execute_checkpoint_check(&self, call: &ToolCall) -> ToolResult {
        if let Err(err) = serde_json::from_value::<CheckpointCheckArgs>(call.arguments.clone()) {
            return tool_arg_error(call, err);
        }
        let Some(checkpoints) = self.checkpoints.as_ref() else {
            return checkpoints_disabled_result(call);
        };
        match checkpoints.integrity_report() {
            Ok(report) => make_result(
                call,
                if report.ok {
                    ToolStatus::Success
                } else {
                    ToolStatus::Stale
                },
                json!({ "integrity": report }),
                ToolCostHint::default(),
                None,
            ),
            Err(err) => tool_error(call, err),
        }
    }

    fn assess_rollback_paths(
        &self,
        call: &ToolCall,
        target: RollbackTarget<'_>,
    ) -> Option<ToolResult> {
        let checkpoints = self.checkpoints.as_ref()?;
        let paths = match checkpoints.rollback_paths(target) {
            Ok(paths) => paths,
            Err(err) => return Some(tool_error(call, err)),
        };
        for path in paths {
            if let Err(err) = safety::assess_write_path(&path, &self.root, &self.shell_sandbox) {
                return Some(make_result(
                    call,
                    ToolStatus::Denied,
                    json!({
                        "error": err.message(),
                        "path": path,
                        "reason": err.code(),
                        "permission_denied": true,
                        "policy_denied": true,
                    }),
                    ToolCostHint::default(),
                    None,
                ));
            }
        }
        None
    }
}

fn rollback_message(result: &squeezy_vcs::RollbackResult) -> Option<&'static str> {
    if result.conflicts.is_empty() {
        return None;
    }
    if result.conflicts.iter().any(|conflict| conflict.retryable) {
        Some(
            "rollback hit retryable filesystem conflicts; close editors or terminals holding the files, pause OneDrive/Defender sync if needed, then retry /undo or inspect affected paths with checkpoint_show",
        )
    } else if result.conflicts.iter().any(|conflict| {
        conflict.reason_code == Some(squeezy_vcs::RollbackConflictReason::GitFilterOrEolMismatch)
    }) {
        Some(
            "rollback detected a Git filter/eol hash-basis mismatch; compare checkpoint blob hashes with current worktree byte hashes before retrying",
        )
    } else {
        Some("rollback conflicts left current file content untouched")
    }
}
