use super::ssh_config_dependency_paths;
use std::fs;
use std::path::PathBuf;

fn make_temp_dir(suffix: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "squeezy_ssh_config_test_{suffix}_{pid}",
        pid = std::process::id()
    ));
    fs::create_dir_all(&base).expect("create temp dir");
    base
}

fn cleanup(p: &PathBuf) {
    let _ = fs::remove_dir_all(p);
}

#[test]
fn collects_identity_file_paths() {
    let tmp = make_temp_dir("identity");
    let home = tmp.join("home");
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).expect("create .ssh");
    let key_file = home.join(".keys").join("id_ed25519");
    fs::create_dir_all(key_file.parent().unwrap()).expect("create .keys");
    fs::write(&key_file, "").expect("write key");
    fs::write(
        ssh_dir.join("config"),
        "Host dev\n  IdentityFile ~/.keys/id_ed25519\n",
    )
    .expect("write config");

    let result = ssh_config_dependency_paths(Some(&home));

    assert!(
        result.contains(&ssh_dir.join("config")),
        "config itself should be present"
    );
    assert!(
        result.iter().any(|p| p.ends_with(".keys/id_ed25519")),
        "IdentityFile should be collected; got {result:?}"
    );

    cleanup(&tmp);
}

#[test]
fn none_home_returns_empty() {
    let result = ssh_config_dependency_paths(None);
    assert!(result.is_empty());
}

#[test]
fn missing_config_returns_config_path_only() {
    let tmp = make_temp_dir("missing");
    let home = tmp.join("home");
    // Don't create .ssh/config — just the home dir.
    fs::create_dir_all(&home).expect("create home");

    let result = ssh_config_dependency_paths(Some(&home));

    // Should contain just the config path even when it doesn't exist.
    assert_eq!(result.len(), 1);
    assert!(result[0].ends_with(".ssh/config"));

    cleanup(&tmp);
}

#[test]
fn recursively_follows_include_directives() {
    let tmp = make_temp_dir("include");
    let home = tmp.join("home");
    let ssh_dir = home.join(".ssh");
    let conf_d = ssh_dir.join("conf.d");
    fs::create_dir_all(&conf_d).expect("create conf.d");
    fs::write(ssh_dir.join("config"), "Include conf.d/devbox.conf\n").expect("write config");
    let key = home.join(".included_key");
    fs::write(&key, "").expect("write key");
    fs::write(
        conf_d.join("devbox.conf"),
        "Host devbox\n  IdentityFile ~/.included_key\n",
    )
    .expect("write include");

    let result = ssh_config_dependency_paths(Some(&home));

    assert!(
        result.iter().any(|p| p.ends_with("devbox.conf")),
        "included file should be collected; got {result:?}"
    );
    assert!(
        result.iter().any(|p| p.ends_with(".included_key")),
        "IdentityFile from included config should be collected; got {result:?}"
    );

    cleanup(&tmp);
}
