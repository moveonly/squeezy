use crate::{CheckpointFile, CheckpointRecord, DiffFileStatus, RollbackResult};

pub(crate) fn rollback_file_has_conflict(
    result: &RollbackResult,
    record: &CheckpointRecord,
    file: &CheckpointFile,
) -> bool {
    result.conflicts.iter().any(|conflict| {
        conflict.checkpoint_id == record.id
            && (conflict.path == file.path
                || file
                    .from_path
                    .as_deref()
                    .is_some_and(|from_path| conflict.path == from_path))
    })
}

pub(crate) fn rollback_write_paths(file: &CheckpointFile) -> Vec<String> {
    let mut paths = if file.status == DiffFileStatus::Renamed {
        let mut paths = vec![file.path.clone()];
        if let Some(from_path) = file.from_path.clone() {
            paths.push(from_path);
        }
        paths
    } else {
        vec![file.path.clone()]
    };
    if let Some(group) = file.before_hardlink_paths.as_ref() {
        paths.extend(group.iter().cloned());
    }
    paths.sort();
    paths.dedup();
    paths
}
