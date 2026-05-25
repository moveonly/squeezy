use super::*;

#[test]
fn persist_permission_rule_dedups_same_triple() {
    let root = std::env::temp_dir().join(format!(
        "squeezy_rule_persist_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("settings.toml");
    let rule = PermissionRule::new(
        "network",
        "domain:docs.rs",
        PermissionAction::Allow,
        PermissionRuleSource::User,
        Some("test".to_string()),
    );
    assert!(persist_permission_rule(&path, &rule).unwrap());
    assert!(!persist_permission_rule(&path, &rule).unwrap());
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("[[permissions.rules]]").count(), 1);
}

#[test]
fn persist_permission_rule_serializes_concurrent_writers() {
    let root = std::env::temp_dir().join(format!(
        "squeezy_rule_persist_concurrent_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("settings.toml");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let mut handles = Vec::new();

    for index in 0..4 {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let rule = PermissionRule::new(
                "network",
                format!("domain:{index}.example"),
                PermissionAction::Allow,
                PermissionRuleSource::Project,
                Some("test".to_string()),
            );
            barrier.wait();
            persist_permission_rule(&path, &rule).unwrap();
        }));
    }

    for handle in handles {
        handle.join().expect("writer thread");
    }
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("[[permissions.rules]]").count(), 4);
    for index in 0..4 {
        assert!(text.contains(&format!("target = \"domain:{index}.example\"")));
    }
}
