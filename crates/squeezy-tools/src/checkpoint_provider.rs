//! Extension surface for pre/post-edit checkpoint capture.
//!
//! Edit-bearing tools (`write_file`, `apply_patch`, `notebook_edit`, and
//! checkpoint-eligible `shell` calls) ask the registry for a snapshot
//! before mutating the worktree and hand it back afterwards to record the
//! delta. By default the registry routes those calls into the
//! journal-backed [`JournalCheckpointProvider`], but external integrations
//! — for example a git-stash-based snapshotter — can implement
//! [`CheckpointProvider`] and register through
//! `ToolRegistry::register_checkpoint_provider` without forking the core
//! dispatch path.
//!
//! The pre-edit snapshot is intentionally opaque: providers stuff any
//! state they need into [`CheckpointSnapshot`] and downcast in
//! `after_edit`. The registry never inspects the payload, which keeps the
//! trait stable as new provider shapes (snapshots that hold a stash hash,
//! a content-addressed blob set, etc.) come online.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use serde_json::{Value, json};
use squeezy_core::{Result, SqueezyError};
use squeezy_vcs::{CheckpointRecord, CheckpointStore, WorkspaceSnapshot};

/// Per-call context handed to a [`CheckpointProvider::after_edit`] call.
///
/// Fields mirror the metadata the journal provider already records, so an
/// external impl that wants to write into a parallel store does not need
/// to scrape `ToolCall` ad hoc.
#[derive(Debug, Clone)]
pub struct CheckpointEditContext {
    /// Name of the tool that produced the mutation (e.g. `"apply_patch"`).
    pub tool_name: String,
    /// Per-call id assigned by the agent loop. Stable across retries of
    /// the same logical step.
    pub call_id: String,
    /// Turn-scoped group id; multiple tool calls in the same turn share
    /// the same `group_id` so a UI can collapse them.
    pub group_id: String,
    /// Outcome label for the tool result. Mirrors the
    /// `ToolStatus`-derived string the journal already persists
    /// (`"success"`, `"error"`, `"denied"`, `"stale"`, `"cancelled"`).
    pub status: &'static str,
    /// Optional warnings the tool wants attached to the checkpoint
    /// record (e.g. `apply_patch` impact-locality misses).
    pub coverage_warnings: Vec<String>,
}

/// Opaque pre-edit snapshot produced by [`CheckpointProvider::before_edit`].
///
/// The registry treats the contents as a black box and hands the same
/// reference back to `after_edit`. Providers downcast to whatever payload
/// they emitted; mismatched downcasts must surface a descriptive error so
/// an accidentally-mixed provider chain is debuggable.
pub struct CheckpointSnapshot {
    inner: Box<dyn Any + Send + Sync>,
}

impl CheckpointSnapshot {
    pub fn new<T: Any + Send + Sync + 'static>(value: T) -> Self {
        Self {
            inner: Box::new(value),
        }
    }

    pub fn downcast_ref<T: Any + 'static>(&self) -> Option<&T> {
        self.inner.downcast_ref::<T>()
    }
}

impl fmt::Debug for CheckpointSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointSnapshot").finish_non_exhaustive()
    }
}

/// Pluggable bridge between the registry's edit-bearing tools and a
/// snapshot/restore backend. The built-in [`JournalCheckpointProvider`]
/// captures `git write-tree` snapshots into a shadow repo; alternative
/// impls can use `git stash`, content-addressed blob stores, or a
/// completely separate VCS without modifying the registry.
pub trait CheckpointProvider: Send + Sync {
    /// Capture a pre-edit snapshot.
    ///
    /// Returning `Ok(None)` opts out of the post-edit step (no
    /// `after_edit` call, no `checkpoint` field on the tool result).
    /// Errors are surfaced through `tool_error` at the call site, which
    /// aborts the in-flight tool call.
    fn before_edit(&self) -> Result<Option<CheckpointSnapshot>>;

    /// Record the delta between the pre-edit snapshot and the current
    /// worktree state.
    ///
    /// * `Ok(Some(value))` attaches `value` to the tool result content
    ///   under `"checkpoint"`.
    /// * `Ok(None)` means "no observable change, attach nothing".
    /// * `Err(_)` is surfaced under `"checkpoint_error"` so the model can
    ///   see why the bridge failed without losing the tool result.
    fn after_edit(
        &self,
        before: &CheckpointSnapshot,
        context: &CheckpointEditContext,
    ) -> Result<Option<Value>>;
}

/// Default journal-backed [`CheckpointProvider`].
///
/// Wraps the per-workspace [`CheckpointStore`] (shadow git repo plus
/// JSONL journal) the registry already owns. Kept as a separate impl so
/// the trait dispatch path and the journal storage layer evolve
/// independently — alternative providers cannot reach into journal
/// internals, and the journal can change its on-disk layout without
/// touching the trait.
pub struct JournalCheckpointProvider {
    store: Arc<CheckpointStore>,
}

impl JournalCheckpointProvider {
    pub fn new(store: Arc<CheckpointStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<CheckpointStore> {
        &self.store
    }
}

impl CheckpointProvider for JournalCheckpointProvider {
    fn before_edit(&self) -> Result<Option<CheckpointSnapshot>> {
        let snapshot = self.store.track_tree()?;
        Ok(Some(CheckpointSnapshot::new(snapshot)))
    }

    fn after_edit(
        &self,
        before: &CheckpointSnapshot,
        context: &CheckpointEditContext,
    ) -> Result<Option<Value>> {
        let before = before.downcast_ref::<WorkspaceSnapshot>().ok_or_else(|| {
            SqueezyError::Tool(
                "checkpoint snapshot was produced by a different provider; \
                 cannot reconcile post-edit state in JournalCheckpointProvider"
                    .to_string(),
            )
        })?;
        let record = self.store.create_checkpoint(
            before,
            &context.tool_name,
            &context.call_id,
            &context.group_id,
            context.status,
            context.coverage_warnings.clone(),
        )?;
        Ok(record.as_ref().map(checkpoint_record_to_json))
    }
}

/// Render a [`CheckpointRecord`] in the JSON shape the registry has been
/// emitting since the journal was the only producer. Kept on the
/// provider side so external impls can match the shape one-to-one when
/// they want to interoperate with existing UI code paths.
pub fn checkpoint_record_to_json(record: &CheckpointRecord) -> Value {
    let mut value = json!({
        "id": record.id,
        "group_id": record.group_id,
        "tool_name": record.tool_name,
        "call_id": record.call_id,
        "status": record.status,
        "summary": record.summary,
        "files": record.files,
    });
    if let Some(object) = value.as_object_mut() {
        if !record.skipped_files.is_empty() {
            object.insert("skipped_files".to_string(), json!(record.skipped_files));
        }
        if !record.coverage_warnings.is_empty() {
            object.insert(
                "coverage_warnings".to_string(),
                json!(record.coverage_warnings),
            );
        }
    }
    value
}
