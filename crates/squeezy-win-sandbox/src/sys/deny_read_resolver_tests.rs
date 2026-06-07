use super::resolve_deny_read_paths;
use std::fs;
use std::path::PathBuf;

/// Create a unique temporary directory under `std::env::temp_dir()` and
/// return its path.  The caller is responsible for cleanup.
fn make_temp_dir(suffix: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "squeezy_deny_read_test_{suffix}_{pid}",
        pid = std::process::id()
    ));
    fs::create_dir_all(&base).expect("create temp dir");
    base
}

fn cleanup(p: &PathBuf) {
    let _ = fs::remove_dir_all(p);
}

#[test]
fn explicit_paths_are_included_verbatim() {
    let tmp = make_temp_dir("explicit");
    let explicit_file = tmp.join("secret.txt");
    fs::write(&explicit_file, "data").expect("write file");

    let result = resolve_deny_read_paths(&[], std::slice::from_ref(&explicit_file), None, &tmp);
    assert!(
        result.contains(&explicit_file),
        "expected explicit file in result; got {result:?}"
    );

    cleanup(&tmp);
}

#[test]
fn relative_glob_expands_under_home() {
    let tmp = make_temp_dir("home_glob");
    let home = tmp.join("home");
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).expect("create .ssh");
    let id_rsa = ssh_dir.join("id_rsa");
    fs::write(&id_rsa, "private key").expect("write id_rsa");

    let patterns = vec![".ssh/**".to_string()];
    let result = resolve_deny_read_paths(&patterns, &[], Some(&home), &tmp);

    // id_rsa should be found under home/.ssh/
    assert!(
        result.contains(&id_rsa),
        "expected id_rsa in result; got {result:?}"
    );

    cleanup(&tmp);
}

#[test]
fn duplicates_are_deduped() {
    let tmp = make_temp_dir("dedup");
    let f = tmp.join("dup.txt");
    fs::write(&f, "data").expect("write file");

    // Pass the same file both as explicit and as a pattern.
    let patterns = vec!["dup.txt".to_string()];
    let result = resolve_deny_read_paths(&patterns, std::slice::from_ref(&f), None, &tmp);

    let count = result.iter().filter(|p| *p == f).count();
    assert_eq!(
        count, 1,
        "expected exactly one entry for dup.txt; got {result:?}"
    );

    cleanup(&tmp);
}
