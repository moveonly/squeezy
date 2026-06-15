use squeezy_core::FileId;
use squeezy_workspace::{IndexCoverage, PathConflict};

use crate::{GraphStats, LanguageReport, RefreshReport};

pub(crate) struct SkippedRefreshInput {
    pub duration_ms: u128,
    pub files_seen: usize,
    pub excluded_files: usize,
    pub excluded_dirs: usize,
    pub excluded_bytes: u64,
    pub path_conflicts: Vec<PathConflict>,
    pub coverage: IndexCoverage,
    pub bytes_seen: u64,
    pub language: LanguageReport,
    pub stats: GraphStats,
}

pub(crate) fn skipped_refresh_report(input: SkippedRefreshInput) -> RefreshReport {
    RefreshReport {
        refreshed: false,
        changed_files: Vec::<FileId>::new(),
        removed_files: Vec::<FileId>::new(),
        reparsed_files: 0,
        changed_paths_from_events: 0,
        changed_paths_from_polling: 0,
        unchanged_event_paths: 0,
        duration_ms: input.duration_ms,
        files_seen: input.files_seen,
        excluded_files: input.excluded_files,
        excluded_dirs: input.excluded_dirs,
        excluded_bytes: input.excluded_bytes,
        path_conflicts: input.path_conflicts,
        coverage: input.coverage,
        bytes_seen: input.bytes_seen,
        bytes_reparsed: 0,
        language: input.language,
        stats: input.stats,
        skipped_due_to_interval: true,
        budget_exhausted: false,
    }
}
