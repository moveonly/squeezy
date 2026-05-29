use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use squeezy_core::Result;

use crate::migrations::{
    InitializeStoreSchemaV1, Migration, MigrationRegistry, V2AddResolverTables, default_registry,
    run_registry,
};

fn temp_workspace(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "squeezy-store-migrations-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("create temp workspace");
    path
}

#[derive(Clone)]
struct RecordingMigration {
    version: u64,
    recorder: Arc<Mutex<Vec<u64>>>,
}

impl RecordingMigration {
    fn new(version: u64, recorder: Arc<Mutex<Vec<u64>>>) -> Self {
        Self { version, recorder }
    }
}

impl Migration for RecordingMigration {
    fn version(&self) -> u64 {
        self.version
    }

    fn migrate(&self, _cwd: &Path) -> Result<()> {
        self.recorder.lock().unwrap().push(self.version);
        Ok(())
    }
}

#[test]
fn run_migrations_is_no_op_when_already_at_target() {
    let cwd = temp_workspace("noop-at-target");
    let registry = default_registry();
    let target = registry.target_version();
    assert!(
        target >= 1,
        "default registry should ship at least the v1 initialiser",
    );
    let first = run_registry(&registry, &cwd).expect("first run bootstraps store");
    assert_eq!(
        first as u64, target,
        "first run on a fresh workspace should apply every registered migration",
    );
    let second = run_registry(&registry, &cwd).expect("second run reads target version from store");
    assert_eq!(
        second, 0,
        "second run must be a no-op once the on-disk schema is at the target version",
    );
}

#[test]
fn run_migrations_applies_missing_migrations_in_ascending_order() {
    let cwd = temp_workspace("missing-in-order");
    let recorder = Arc::new(Mutex::new(Vec::<u64>::new()));
    let mut registry = MigrationRegistry::new();
    registry
        .register(RecordingMigration::new(7, Arc::clone(&recorder)))
        .register(RecordingMigration::new(2, Arc::clone(&recorder)))
        .register(RecordingMigration::new(5, Arc::clone(&recorder)));

    assert_eq!(
        registry.len(),
        3,
        "registry should hold every distinct version registered",
    );
    assert_eq!(
        registry.target_version(),
        7,
        "target version must follow the highest registered migration version",
    );

    let applied = run_registry(&registry, &cwd).expect("run recording migrations");

    assert_eq!(
        applied, 3,
        "every registered migration should run from version 0"
    );
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![2, 5, 7],
        "migrations must run in ascending version order regardless of registration order",
    );
}

#[test]
#[should_panic(expected = "duplicate migration version")]
fn registering_duplicate_versions_panics() {
    let recorder = Arc::new(Mutex::new(Vec::<u64>::new()));
    let mut registry = MigrationRegistry::new();
    registry.register(RecordingMigration::new(3, Arc::clone(&recorder)));
    registry.register(RecordingMigration::new(3, Arc::clone(&recorder)));
}

#[test]
fn default_registry_target_version_matches_schema_constant() {
    assert_eq!(
        default_registry().target_version(),
        crate::SCHEMA_VERSION,
        "the highest registered migration must land the store at SCHEMA_VERSION",
    );
}

#[test]
fn v1_and_v2_migrations_are_distinct() {
    assert_eq!(InitializeStoreSchemaV1.version(), 1);
    assert_eq!(V2AddResolverTables.version(), 2);
}
