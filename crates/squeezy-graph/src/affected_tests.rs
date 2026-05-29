use std::collections::{HashMap, HashSet};

use squeezy_core::FileId;

use super::compute_affected;

fn fid(id: &str) -> FileId {
    FileId::new(id)
}

#[test]
fn leaf_change_alone_does_not_invalidate_importers() {
    let changed: HashSet<FileId> = [fid("leaf")].into_iter().collect();
    let importers: HashMap<FileId, Vec<FileId>> =
        [(fid("leaf"), vec![fid("caller")])].into_iter().collect();
    let propagating: HashSet<FileId> = HashSet::new();
    let removed: HashSet<FileId> = HashSet::new();

    let affected = compute_affected(&changed, &importers, &propagating, &removed);
    assert_eq!(affected, [fid("leaf")].into_iter().collect::<HashSet<_>>());
}

#[test]
fn export_change_propagates_to_every_reverse_reachable_file() {
    let changed: HashSet<FileId> = [fid("base")].into_iter().collect();
    let importers: HashMap<FileId, Vec<FileId>> = [
        (fid("base"), vec![fid("mid")]),
        (fid("mid"), vec![fid("leaf-a"), fid("leaf-b")]),
    ]
    .into_iter()
    .collect();
    let propagating: HashSet<FileId> = [fid("base")].into_iter().collect();
    let removed: HashSet<FileId> = HashSet::new();

    let affected = compute_affected(&changed, &importers, &propagating, &removed);
    let expected: HashSet<FileId> = [fid("base"), fid("mid"), fid("leaf-a"), fid("leaf-b")]
        .into_iter()
        .collect();
    assert_eq!(affected, expected);
}

#[test]
fn removed_files_propagate_even_without_being_in_changed() {
    let changed: HashSet<FileId> = HashSet::new();
    let importers: HashMap<FileId, Vec<FileId>> =
        [(fid("gone"), vec![fid("caller")])].into_iter().collect();
    let propagating: HashSet<FileId> = HashSet::new();
    let removed: HashSet<FileId> = [fid("gone")].into_iter().collect();

    let affected = compute_affected(&changed, &importers, &propagating, &removed);
    let expected: HashSet<FileId> = [fid("gone"), fid("caller")].into_iter().collect();
    assert_eq!(affected, expected);
}

#[test]
fn import_cycle_terminates_without_infinite_loop() {
    let changed: HashSet<FileId> = [fid("a")].into_iter().collect();
    let importers: HashMap<FileId, Vec<FileId>> = [
        (fid("a"), vec![fid("b")]),
        (fid("b"), vec![fid("a"), fid("c")]),
    ]
    .into_iter()
    .collect();
    let propagating: HashSet<FileId> = [fid("a")].into_iter().collect();
    let removed: HashSet<FileId> = HashSet::new();

    let affected = compute_affected(&changed, &importers, &propagating, &removed);
    let expected: HashSet<FileId> = [fid("a"), fid("b"), fid("c")].into_iter().collect();
    assert_eq!(affected, expected);
}
