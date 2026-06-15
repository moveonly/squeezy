use std::collections::BTreeSet;
use std::fs;

use serde::{Deserialize, Serialize};
use squeezy_core::{Result, SqueezyError};

use crate::fs_util;

use super::{
    ARCHIVED_SUBDIR, SessionQuery, SessionStatus, SessionStore, deserialize_session_metadata,
    enrich_lock_error, now_ms,
};

impl SessionStore {
    /// Move a session out of the live root into `archived/<id>/` and flip
    /// its metadata status to `Archived`. Archived sessions are excluded
    /// from `list` (unless `include_archived` is true) and skipped by
    /// `cleanup` retention sweeps. The on-disk session id is preserved so
    /// `unarchive_session` is symmetric.
    pub fn archive_session(&self, session_id: &str) -> Result<()> {
        let src = self.session_dir(session_id);
        if !src.exists() {
            return Err(SqueezyError::Tool(format!(
                "archive_session: session {session_id} not found at {}",
                src.display()
            )));
        }
        let archived_root = self.root.join(ARCHIVED_SUBDIR);
        fs::create_dir_all(&archived_root)?;
        let dest = archived_root.join(session_id);
        if dest.exists() {
            return Err(SqueezyError::Tool(format!(
                "archive_session: archived session already exists at {}",
                dest.display()
            )));
        }
        fs_util::move_path(&src, &dest)?;
        let metadata_path = dest.join("metadata.json");
        if let Ok(text) = fs::read_to_string(&metadata_path)
            && let Ok(mut metadata) = deserialize_session_metadata(&text)
        {
            let stamp = now_ms();
            metadata.status = SessionStatus::Archived;
            metadata.archived_at_ms = Some(stamp);
            if metadata.ended_at_ms.is_none() {
                metadata.ended_at_ms = Some(stamp);
            }
            let _ = self.write_metadata_file(&dest, &metadata);
            Self::record_global_index(&metadata);
        }
        Ok(())
    }

    /// Reverse of [`archive_session`]. Moves the session back to the live
    /// root and restores the metadata status to `Completed`.
    pub fn unarchive_session(&self, session_id: &str) -> Result<()> {
        let src = self.root.join(ARCHIVED_SUBDIR).join(session_id);
        if !src.exists() {
            return Err(SqueezyError::Tool(format!(
                "unarchive_session: archived session {session_id} not found"
            )));
        }
        let dest = self.session_dir(session_id);
        if dest.exists() {
            return Err(SqueezyError::Tool(format!(
                "unarchive_session: a live session already exists at {}",
                dest.display()
            )));
        }
        fs_util::move_path(&src, &dest)?;
        let metadata_path = dest.join("metadata.json");
        if let Ok(text) = fs::read_to_string(&metadata_path)
            && let Ok(mut metadata) = deserialize_session_metadata(&text)
        {
            metadata.status = SessionStatus::Completed;
            metadata.archived_at_ms = None;
            let _ = self.write_metadata_file(&dest, &metadata);
            Self::record_global_index(&metadata);
        }
        Ok(())
    }

    /// True when `session_id` lives only under the `archived/` subtree -
    /// i.e. there is no live-root directory for it but an archived one
    /// exists. The resolver and metadata reader resolve through
    /// `locate_session_dir` (live + archived), but the writer and
    /// `SessionHandle::dir` use the live root only, so resume must revive
    /// an archived session before opening it.
    pub fn is_archived(&self, session_id: &str) -> bool {
        !self.session_dir(session_id).exists()
            && self.root.join(ARCHIVED_SUBDIR).join(session_id).exists()
    }

    pub fn cleanup(&self, ids: &[String]) -> Result<CleanupReport> {
        self.cleanup_excluding(ids, None)
    }

    /// Like [`cleanup`] but skips `protected_id` even if it would otherwise
    /// match (used to keep the currently active session from being removed
    /// out from under a live agent).
    ///
    /// Defaults to [`CleanupMode::Archive`]: live sessions that expire or are
    /// explicitly named in `ids` are moved into `archived/<id>/`. Use
    /// [`Self::cleanup_with`] with [`CleanupMode::Purge`] to hard-delete
    /// instead.
    pub fn cleanup_excluding(
        &self,
        ids: &[String],
        protected_id: Option<&str>,
    ) -> Result<CleanupReport> {
        self.cleanup_with(ids, protected_id, CleanupMode::Archive)
    }

    /// Run the cleanup sweep with explicit control over the soft-archive vs
    /// hard-delete decision.
    ///
    /// [`CleanupMode::Archive`] (the default) moves expired or explicitly
    /// named live sessions into `archived/<id>/` and flips their status to
    /// [`SessionStatus::Archived`]. They survive until the archive retention
    /// sweep removes them after `retention_archive_days`. This gives users a
    /// window to recover a session that the retention policy would otherwise
    /// destroy: live retention reduces disk pressure, archive retention
    /// bounds the recoverable history. Setting `retention_archive_days` to
    /// `0` disables the archive sweep so archived sessions are kept until
    /// the user removes them by hand.
    ///
    /// [`CleanupMode::Purge`] skips the soft-archive step and hard-deletes
    /// live sessions outright. Sessions already in `archived/<id>/` are also
    /// hard-deleted irrespective of `retention_archive_days`, so `--purge`
    /// is the explicit "I want this gone" escape hatch from the
    /// archive-by-default policy.
    pub fn cleanup_with(
        &self,
        ids: &[String],
        protected_id: Option<&str>,
        mode: CleanupMode,
    ) -> Result<CleanupReport> {
        let mut archived = Vec::new();
        let mut removed = Vec::new();
        let cutoff = now_ms().saturating_sub(self.retention_days.saturating_mul(86_400_000));
        let explicit: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        for metadata in self.list(&SessionQuery {
            include_archived: true,
            ..SessionQuery::default()
        })? {
            if protected_id == Some(metadata.session_id.as_str()) {
                continue;
            }
            if matches!(metadata.status, SessionStatus::Archived) {
                let is_explicit = explicit.contains(metadata.session_id.as_str());
                // `--purge` hard-deletes archived sessions regardless of
                // archive retention so the user has an explicit "I want
                // this gone now" path. The `Archive` default mode keeps
                // them around until the retention sweep removes them.
                let force_remove = matches!(mode, CleanupMode::Purge) && is_explicit;
                if force_remove {
                    let dir = self.root.join(ARCHIVED_SUBDIR).join(&metadata.session_id);
                    if dir.exists() {
                        fs::remove_dir_all(&dir).map_err(|e| enrich_lock_error(e, &dir))?;
                    }
                    self.remove_session_index_entry(&metadata.session_id);
                    removed.push(metadata.session_id);
                    continue;
                }
                if self.retention_archive_days == 0 {
                    continue;
                }
                let archive_cutoff =
                    now_ms().saturating_sub(self.retention_archive_days.saturating_mul(86_400_000));
                // Prefer the dedicated archival timestamp. Older metadata
                // files written before `archived_at_ms` existed fall back
                // to `ended_at_ms` (set when `archive_session` flips the
                // status) and finally `started_at_ms` so the sweep keeps
                // working on legacy on-disk data.
                let archived_at = metadata
                    .archived_at_ms
                    .or(metadata.ended_at_ms)
                    .unwrap_or(metadata.started_at_ms);
                if archived_at < archive_cutoff {
                    let dir = self.root.join(ARCHIVED_SUBDIR).join(&metadata.session_id);
                    if dir.exists() {
                        fs::remove_dir_all(&dir).map_err(|e| enrich_lock_error(e, &dir))?;
                    }
                    self.remove_session_index_entry(&metadata.session_id);
                    removed.push(metadata.session_id);
                }
                continue;
            }
            let is_explicit = explicit.contains(metadata.session_id.as_str());
            // Never sweep a `Running` session through retention alone: it may
            // belong to a long-lived process whose `ended_at_ms` simply isn't
            // set yet. Explicit ids still win so users can force-archive a
            // crashed or stuck session.
            let expired = match metadata.ended_at_ms {
                Some(end) => end < cutoff,
                None => {
                    !matches!(metadata.status, SessionStatus::Running)
                        && metadata.started_at_ms < cutoff
                }
            };
            if is_explicit || expired {
                match mode {
                    CleanupMode::Archive => {
                        // `archive_session` is idempotent for the live ->
                        // archived move; a destination collision means
                        // another caller raced us, which we surface so the
                        // operator can investigate.
                        self.archive_session(&metadata.session_id)?;
                        archived.push(metadata.session_id);
                    }
                    CleanupMode::Purge => {
                        let dir = self.session_dir(&metadata.session_id);
                        if dir.exists() {
                            fs::remove_dir_all(&dir).map_err(|e| enrich_lock_error(e, &dir))?;
                        }
                        self.remove_session_index_entry(&metadata.session_id);
                        removed.push(metadata.session_id);
                    }
                }
            }
        }
        Ok(CleanupReport { archived, removed })
    }

    /// Soft-delete that prefers archiving over permanent removal.
    /// Live sessions are moved into `archived/<id>/` (same path as
    /// [`archive_session`]); archived sessions are left in place because
    /// the retention sweep is the only path that permanently deletes
    /// history. Missing sessions are a no-op so callers can drive this
    /// from a stale id without erroring.
    pub fn remove_session(&self, session_id: &str) -> Result<()> {
        let live_dir = self.session_dir(session_id);
        if live_dir.exists() {
            return self.archive_session(session_id);
        }
        // Already archived (or never existed) - nothing to do. The
        // archive retention sweep handles the eventual hard delete.
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    /// Live sessions that were moved into `archived/<id>/` by this
    /// sweep. They still exist on disk and can be restored with
    /// [`SessionStore::unarchive_session`] until the archive retention
    /// sweep deletes them.
    #[serde(default)]
    pub archived: Vec<String>,
    /// Sessions that were permanently deleted by this sweep. Populated
    /// when the archive retention sweep removes a session that has
    /// outlived `retention_archive_days`, and when [`CleanupMode::Purge`]
    /// is requested for explicit `ids`.
    pub removed: Vec<String>,
}

/// Soft-archive vs hard-delete policy for [`SessionStore::cleanup_with`].
/// The CLI surfaces this as `squeezy sessions cleanup --archive` (default)
/// vs `--purge`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CleanupMode {
    /// Move expired or explicitly named live sessions into
    /// `archived/<id>/` rather than deleting them. The archive retention
    /// sweep eventually removes them after `retention_archive_days`.
    #[default]
    Archive,
    /// Hard-delete expired or explicitly named sessions. Live sessions
    /// skip the archive step; already-archived sessions named in `ids`
    /// are removed without waiting for archive retention.
    Purge,
}
