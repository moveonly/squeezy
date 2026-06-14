use std::path::{Path, PathBuf};

use super::*;

/// Serialize `HOME` mutation across the suite and run `body` with `HOME`
/// pointed at `home`, restoring the prior value afterward.
fn with_home<R>(home: &Path, body: impl FnOnce() -> R) -> R {
    let _guard = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("HOME");
    // SAFETY: the lock above serialises HOME mutation across the suite.
    unsafe {
        std::env::set_var("HOME", home);
    }
    let result = body();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    result
}

/// Run `body` with a [`Memory`] backed by a temp global (HOME) base and a temp
/// project (workspace) base, passing both base dirs for path assertions.
fn with_memory<R>(label: &str, body: impl FnOnce(&Memory, &Path, &Path) -> R) -> R {
    let home = temp_home(&format!("{label}-home"));
    let workspace = temp_home(&format!("{label}-ws"));
    with_home(&home, || {
        let mem = Memory::new(Some(&workspace));
        body(&mem, &home, &workspace)
    })
}

fn temp_home(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-memory-{label}-{}-{}",
        std::process::id(),
        unique()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir temp dir");
    dir
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn type_routes_to_scope() {
    assert_eq!(MemoryType::User.scope(), Scope::Global);
    assert_eq!(MemoryType::Feedback.scope(), Scope::Global);
    assert_eq!(MemoryType::Project.scope(), Scope::Project);
    assert_eq!(MemoryType::Reference.scope(), Scope::Project);
}

#[test]
fn save_global_writes_to_home_base() {
    with_memory("global", |mem, home, workspace| {
        let saved = mem
            .save(
                "prefers-bun-over-npm",
                MemoryType::Feedback,
                "Use bun, not npm.",
                "Use bun for all package scripts.\n\n**Why:** npm is slow here.",
                None,
                None,
            )
            .expect("save");
        assert_eq!(saved.scope, Scope::Global);
        assert!(
            saved.path.starts_with(home.join(".squeezy").join("memory")),
            "global memory under HOME: {:?}",
            saved.path
        );
        assert!(
            !workspace.join(".squeezy").join("memory").exists(),
            "nothing written to the project base for a global memory"
        );

        let body = std::fs::read_to_string(&saved.path).expect("read");
        assert!(body.contains("name: prefers-bun-over-npm"));
        assert!(body.contains("type: feedback"));

        let index = mem.global_index().expect("idx").expect("present");
        assert!(
            index.contains("](memory/prefers-bun-over-npm.md)"),
            "{index}"
        );
        assert!(mem.project_index().expect("proj idx").is_none());
    });
}

#[test]
fn save_project_writes_to_workspace_and_gitignores() {
    with_memory("project", |mem, home, workspace| {
        let saved = mem
            .save(
                "auth-rewrite",
                MemoryType::Project,
                "compliance-driven",
                "The auth rewrite is driven by legal/compliance, not tech debt.",
                None,
                None,
            )
            .expect("save");
        assert_eq!(saved.scope, Scope::Project);
        assert!(
            saved
                .path
                .starts_with(workspace.join(".squeezy").join("memory")),
            "project memory under workspace: {:?}",
            saved.path
        );
        assert!(
            !home.join(".squeezy").join("memory").exists(),
            "nothing written to the global base for a project memory"
        );

        let gitignore = workspace.join(".squeezy").join(".gitignore");
        assert!(gitignore.exists(), "project .squeezy/ is gitignored");
        assert!(std::fs::read_to_string(&gitignore).unwrap().contains('*'));

        let index = mem.project_index().expect("idx").expect("present");
        assert!(index.contains("](memory/auth-rewrite.md)"), "{index}");
        assert!(mem.global_index().expect("global idx").is_none());
    });
}

#[test]
fn list_spans_both_scopes_tagged() {
    with_memory("list", |mem, _home, _ws| {
        mem.save(
            "who-i-am",
            MemoryType::User,
            "data scientist",
            "body",
            None,
            None,
        )
        .expect("save user");
        mem.save(
            "repo-ctx",
            MemoryType::Project,
            "ongoing migration",
            "body",
            None,
            None,
        )
        .expect("save project");

        let entries = mem.list().expect("list");
        assert_eq!(entries.len(), 2);
        let by_name: std::collections::HashMap<_, _> =
            entries.iter().map(|e| (e.name.as_str(), e.scope)).collect();
        assert_eq!(by_name["who-i-am"], Scope::Global);
        assert_eq!(by_name["repo-ctx"], Scope::Project);
    });
}

#[test]
fn read_searches_project_then_global_and_delete_clears_both() {
    with_memory("read-delete", |mem, _home, _ws| {
        // Same slug in both scopes (user->global, project->project).
        mem.save("ctx", MemoryType::User, "global one", "GLOBAL", None, None)
            .expect("save global");
        mem.save(
            "ctx",
            MemoryType::Project,
            "project one",
            "PROJECT",
            None,
            None,
        )
        .expect("save project");

        let (scope, body) = mem.read("ctx").expect("read").expect("present");
        assert_eq!(scope, Scope::Project, "project shadows global on read");
        assert!(body.contains("PROJECT"));

        assert!(mem.delete("ctx").expect("delete"), "delete reports removal");
        assert!(
            mem.read("ctx").expect("read after").is_none(),
            "gone from both scopes"
        );
        assert!(
            !mem.delete("ctx").expect("delete again"),
            "second delete is a no-op"
        );
    });
}

#[test]
fn save_overwrites_and_upserts_single_index_line() {
    with_memory("upsert", |mem, _home, _ws| {
        mem.save(
            "auth-rewrite",
            MemoryType::Project,
            "first",
            "first body",
            None,
            None,
        )
        .expect("save 1");
        mem.save(
            "auth-rewrite",
            MemoryType::Project,
            "second description",
            "second body",
            Some("Auth rewrite scope"),
            Some("compliance, not tech debt"),
        )
        .expect("save 2");

        let (_, body) = mem.read("auth-rewrite").expect("read").expect("present");
        assert!(body.contains("second body"));
        assert!(!body.contains("first body"));

        let index = mem.project_index().expect("idx").expect("present");
        let pointers: Vec<&str> = index
            .lines()
            .filter(|l| l.contains("(memory/auth-rewrite.md)"))
            .collect();
        assert_eq!(pointers.len(), 1, "single pointer after upsert: {index}");
        assert!(pointers[0].contains("Auth rewrite scope"));
    });
}

#[test]
fn injected_link_in_title_cannot_corrupt_another_memorys_index_entry() {
    with_memory("inject", |mem, _home, _ws| {
        mem.save(
            "victim",
            MemoryType::Project,
            "real one",
            "body",
            None,
            None,
        )
        .expect("save victim");
        mem.save(
            "attacker",
            MemoryType::Project,
            "desc",
            "body",
            Some("Evil](memory/victim.md)"),
            Some("hook ](memory/victim.md) tail"),
        )
        .expect("save attacker");

        let index = mem.project_index().expect("idx").expect("present");
        let attacker_line = index
            .lines()
            .find(|l| l.contains("(memory/attacker.md)"))
            .expect("attacker line");
        assert!(
            !attacker_line.contains("](memory/victim.md)"),
            "brackets stripped, no forged marker: {attacker_line}"
        );

        assert!(mem.delete("victim").expect("delete victim"));
        let index = mem.project_index().expect("idx").expect("present");
        assert!(
            !index.contains("](memory/victim.md)"),
            "victim link gone: {index}"
        );
        assert!(
            index.contains("](memory/attacker.md)"),
            "attacker survives: {index}"
        );
        assert!(
            mem.read("attacker").expect("read").is_some(),
            "attacker file kept"
        );
    });
}

#[test]
fn concurrent_saves_all_land_in_index() {
    with_memory("concurrent", |mem, _home, _ws| {
        const N: usize = 8;
        std::thread::scope(|scope| {
            for i in 0..N {
                let mem = &*mem;
                scope.spawn(move || {
                    let name = format!("fact-{i}");
                    mem.save(&name, MemoryType::User, "d", "body", None, None)
                        .expect("save");
                });
            }
        });
        let index = mem.global_index().expect("idx").expect("present");
        let pointers = index
            .lines()
            .filter(|l| l.contains("(memory/fact-"))
            .count();
        assert_eq!(
            pointers, N,
            "every concurrent save kept its pointer: {index}"
        );
        assert_eq!(mem.list().expect("list").len(), N);
    });
}

#[test]
fn save_rejects_oversized_body() {
    with_memory("oversize", |mem, _home, _ws| {
        let big = "x".repeat(MAX_MEMORY_BODY_BYTES + 1);
        assert!(
            mem.save("big", MemoryType::User, "d", &big, None, None)
                .is_err()
        );
    });
}

#[cfg(not(windows))]
#[test]
fn global_save_without_home_is_a_clean_error() {
    let _guard = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("HOME");
    // SAFETY: serialized by TEST_ENV_LOCK.
    unsafe {
        std::env::remove_var("HOME");
    }
    let mem = Memory::new(None);
    let result = mem.save("homeless", MemoryType::User, "d", "b", None, None);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    assert!(
        result.is_err(),
        "global save without HOME errors, not panics"
    );
}

#[test]
fn project_save_without_workspace_is_a_clean_error() {
    let home = temp_home("no-ws");
    with_home(&home, || {
        let mem = Memory::new(None); // global only — no project base
        let result = mem.save("x", MemoryType::Project, "d", "b", None, None);
        assert!(result.is_err(), "project save with no workspace errors");
    });
}

#[test]
fn name_validation_rejects_traversal_and_bad_chars() {
    assert!(validate_name("../escape").is_err());
    assert!(validate_name("..").is_err());
    assert!(validate_name("has space").is_err());
    assert!(validate_name("Capitalized").is_err());
    assert!(validate_name("").is_err());
    assert!(validate_name("good-name_1").is_ok());

    with_memory("name-canon", |mem, _home, _ws| {
        let saved = mem
            .save("Mixed-Case", MemoryType::User, "d", "b", None, None)
            .expect("save");
        assert_eq!(saved.name, "mixed-case");
    });
}

#[test]
fn memory_type_parse_roundtrip() {
    for ty in [
        MemoryType::User,
        MemoryType::Feedback,
        MemoryType::Project,
        MemoryType::Reference,
    ] {
        assert_eq!(MemoryType::parse(ty.as_str()), Some(ty));
    }
    assert_eq!(MemoryType::parse("USER"), Some(MemoryType::User));
    assert_eq!(MemoryType::parse("nope"), None);
}
