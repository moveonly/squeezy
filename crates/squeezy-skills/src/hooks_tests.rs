use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::{SkillConfigEntry, SkillsConfig};
use squeezy_hooks::{HookEvent, HookRegistry};

use super::*;
use crate::{LoadedSkill, SkillCatalog, SkillContextMode, SkillSource, SkillSummary};

#[cfg(unix)]
static HOOK_ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(unix)]
struct ScopedHookPayloadEnv {
    payload: Option<std::ffi::OsString>,
    payload_file: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl ScopedHookPayloadEnv {
    fn with_stale_values() -> Self {
        let payload = std::env::var_os("SQUEEZY_HOOK_PAYLOAD");
        let payload_file = std::env::var_os("SQUEEZY_HOOK_PAYLOAD_FILE");
        unsafe {
            std::env::set_var("SQUEEZY_HOOK_PAYLOAD", "stale-inline-payload");
            std::env::set_var("SQUEEZY_HOOK_PAYLOAD_FILE", "stale-payload-file");
        }
        Self {
            payload,
            payload_file,
        }
    }
}

#[cfg(unix)]
impl Drop for ScopedHookPayloadEnv {
    fn drop(&mut self) {
        unsafe {
            match &self.payload {
                Some(value) => std::env::set_var("SQUEEZY_HOOK_PAYLOAD", value),
                None => std::env::remove_var("SQUEEZY_HOOK_PAYLOAD"),
            }
            match &self.payload_file {
                Some(value) => std::env::set_var("SQUEEZY_HOOK_PAYLOAD_FILE", value),
                None => std::env::remove_var("SQUEEZY_HOOK_PAYLOAD_FILE"),
            }
        }
    }
}

#[cfg(unix)]
fn sh_quote_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn write_skill(dir: &Path, name: &str, description: &str, triggers: &[&str]) {
    write_skill_with_body(dir, name, description, triggers, &format!("# {name}\n"));
}

fn write_skill_with_body(dir: &Path, name: &str, description: &str, triggers: &[&str], body: &str) {
    fs::create_dir_all(dir).expect("mkdir");
    let triggers = triggers
        .iter()
        .map(|trigger| format!("  - {trigger}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {description}\ntriggers:\n{triggers}\n---\n{body}"
        ),
    )
    .expect("write skill");
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("squeezy_{name}_{nonce}"));
    fs::create_dir_all(&path).expect("create temp workspace");
    path
}

#[test]
fn register_skill_hooks_installs_one_handler_per_spec() {
    let skill = LoadedSkill {
        summary: SkillSummary {
            name: "validator".to_string(),
            description: "d".to_string(),
            when_to_use: None,
            source: SkillSource::Project,
            location: PathBuf::from("/tmp/SKILL.md"),
            disabled: false,
            manifest: None,
            context_mode: SkillContextMode::Inline,
        },
        base_dir: PathBuf::from("/tmp"),
        body: String::new(),
        hooks: BTreeMap::from([(
            HookEvent::PreToolUse,
            vec![SkillHookMatcher {
                matcher: Some("Bash".to_string()),
                hooks: vec![
                    SkillHookSpec {
                        command: "true".to_string(),
                        once: false,
                        timeout_secs: None,
                        fail_open: true,
                        kind_valid: true,
                        failure_policy: HookFailurePolicy::Allow,
                    },
                    SkillHookSpec {
                        command: "true".to_string(),
                        once: true,
                        timeout_secs: None,
                        fail_open: true,
                        kind_valid: true,
                        failure_policy: HookFailurePolicy::Allow,
                    },
                ],
            }],
        )]),
    };
    let mut registry = HookRegistry::new();
    let installed = register_skill_hooks(&skill, &mut registry);
    assert_eq!(installed, 2);
    assert_eq!(registry.len(), 2);
}

#[test]
fn catalog_register_hooks_skips_disabled_and_aggregates() {
    let root = temp_workspace("skills_catalog_register_hooks");
    let user_dir = root.join("user");

    let alpha_dir = user_dir.join("alpha");
    fs::create_dir_all(&alpha_dir).expect("mkdir alpha");
    fs::write(
        alpha_dir.join("SKILL.md"),
        "---\nname: alpha\ndescription: \"a\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"true\"\n---\n# alpha\n",
    )
    .expect("write alpha");

    let beta_dir = user_dir.join("beta");
    fs::create_dir_all(&beta_dir).expect("mkdir beta");
    fs::write(
        beta_dir.join("SKILL.md"),
        "---\nname: beta\ndescription: \"b\"\nhooks:\n  PostToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"true\"\n        - type: command\n          command: \"true\"\n          once: true\n---\n# beta\n",
    )
    .expect("write beta");

    let gamma_dir = user_dir.join("gamma");
    fs::create_dir_all(&gamma_dir).expect("mkdir gamma");
    fs::write(
        gamma_dir.join("SKILL.md"),
        "---\nname: gamma\ndescription: \"g\"\n---\n# gamma\n",
    )
    .expect("write gamma");

    let config = SkillsConfig {
        user_dir,
        compat_user_dir: root.join("compat"),
        config: vec![SkillConfigEntry {
            name: Some("beta".to_string()),
            path: None,
            enabled: false,
        }],
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);

    let mut registry = HookRegistry::new();
    let installed = catalog.register_hooks(&mut registry);
    assert_eq!(
        installed, 1,
        "only the non-disabled skill with hooks should contribute handlers"
    );
    assert_eq!(registry.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_fires_on_matching_event_and_skips_others() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace("skill_hook_fires");
    let marker = root.join("ran");
    let script = root.join("hook.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\necho fired > {}\n", marker.display()),
    )
    .expect("write hook script");
    let mut perms = fs::metadata(&script).expect("script meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod hook");

    let spec = SkillHookSpec {
        command: script.display().to_string(),
        once: false,
        timeout_secs: None,
        fail_open: true,
        kind_valid: true,
        failure_policy: HookFailurePolicy::Allow,
    };
    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::PreToolUse,
        Some("Bash".to_string()),
        spec,
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    // Non-matching event does not run the script.
    let _ = registry.dispatch(squeezy_hooks::HookPayload::PostToolUse {
        turn_id: "1".into(),
        tool_name: "Bash".into(),
        call_id: "c1".into(),
        status: "success".into(),
    });
    assert!(!marker.exists());
    // Matching event with the wrong tool also skips.
    let _ = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
        turn_id: "1".into(),
        tool_name: "Edit".into(),
        call_id: "c2".into(),
    });
    assert!(!marker.exists());
    // Matching event with the matching tool fires.
    let _ = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
        turn_id: "1".into(),
        tool_name: "Bash".into(),
        call_id: "c3".into(),
    });
    assert!(marker.exists(), "expected hook to create marker file");

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_once_self_removes_after_first_run() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace("skill_hook_once");
    let counter = root.join("count");
    fs::write(&counter, "0").expect("init counter");
    let script = root.join("hook.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nn=$(cat {0})\necho $((n + 1)) > {0}\n",
            counter.display()
        ),
    )
    .expect("write hook script");
    let mut perms = fs::metadata(&script).expect("script meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod hook");

    let spec = SkillHookSpec {
        command: script.display().to_string(),
        once: true,
        timeout_secs: None,
        fail_open: true,
        kind_valid: true,
        failure_policy: HookFailurePolicy::Allow,
    };
    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::PreToolUse,
        None,
        spec,
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    for call_id in ["c1", "c2", "c3"] {
        let _ = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
            turn_id: "1".into(),
            tool_name: "Bash".into(),
            call_id: call_id.into(),
        });
    }
    let count = fs::read_to_string(&counter).expect("read counter");
    assert_eq!(count.trim(), "1", "once: true must fire exactly once");

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_once_claims_concurrent_dispatches_atomically() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace("skill_hook_once_concurrent");
    let counter = root.join("count");
    let script = root.join("hook.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nsleep 1\nprintf x >> {}\n", counter.display()),
    )
    .expect("write hook script");
    let mut perms = fs::metadata(&script).expect("script meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod hook");

    let spec = SkillHookSpec {
        command: script.display().to_string(),
        once: true,
        timeout_secs: None,
        fail_open: true,
        kind_valid: true,
        failure_policy: HookFailurePolicy::Allow,
    };
    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::PreToolUse,
        None,
        spec,
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));
    let registry = Arc::new(registry);

    const THREADS: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    let mut handles = Vec::new();
    for index in 0..THREADS {
        let registry = Arc::clone(&registry);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let _ = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
                turn_id: "1".into(),
                tool_name: "Bash".into(),
                call_id: format!("c{index}"),
            });
        }));
    }
    for handle in handles {
        handle.join().expect("dispatch thread should finish");
    }

    let count = fs::read_to_string(&counter).unwrap_or_default().len();
    assert_eq!(
        count, 1,
        "once: true must allow only one concurrent dispatch to execute"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_once_retries_after_failed_first_run() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace("skill_hook_once_retry");
    let counter = root.join("count");
    fs::write(&counter, "0").expect("init counter");
    let marker = root.join("ready");
    // The hook counts every run, but only succeeds (exit 0) once the
    // marker file exists; before that it denies the action with exit 1.
    let script = root.join("hook.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nn=$(cat {0})\necho $((n + 1)) > {0}\n[ -e {1} ]\n",
            counter.display(),
            marker.display()
        ),
    )
    .expect("write hook script");
    let mut perms = fs::metadata(&script).expect("script meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod hook");

    let spec = SkillHookSpec {
        command: script.display().to_string(),
        once: true,
        timeout_secs: None,
        fail_open: true,
        kind_valid: true,
        failure_policy: HookFailurePolicy::Allow,
    };
    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::PreToolUse,
        None,
        spec,
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    // First dispatch: marker absent, so the hook runs, exits non-zero,
    // and denies. A failed run must NOT consume the single fire.
    let first = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
        turn_id: "1".into(),
        tool_name: "Bash".into(),
        call_id: "c1".into(),
    });
    assert!(
        first.iter().any(|r| !r.allow),
        "failed first run should deny"
    );
    assert_eq!(
        fs::read_to_string(&counter).expect("read counter").trim(),
        "1",
        "first run should have executed the command once"
    );

    // Create the marker so the next run would succeed, then re-dispatch:
    // the hook must run again (not be silently skipped) and now allow.
    fs::write(&marker, "").expect("write marker");
    let second = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
        turn_id: "1".into(),
        tool_name: "Bash".into(),
        call_id: "c2".into(),
    });
    assert!(
        second.iter().all(|r| r.allow),
        "successful retry should allow"
    );
    assert_eq!(
        fs::read_to_string(&counter).expect("read counter").trim(),
        "2",
        "failed first run must be retried, so the command runs again"
    );

    // A third dispatch after success must be skipped: the flag is now set.
    let _ = registry.dispatch(squeezy_hooks::HookPayload::PreToolUse {
        turn_id: "1".into(),
        tool_name: "Bash".into(),
        call_id: "c3".into(),
    });
    assert_eq!(
        fs::read_to_string(&counter).expect("read counter").trim(),
        "2",
        "after a successful run the hook self-skips"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_inline_payload_removes_stale_payload_file_env() {
    let _guard = HOOK_ENV_LOCK.lock().expect("hook env lock");
    let _env = ScopedHookPayloadEnv::with_stale_values();
    let root = temp_workspace("skill_hook_inline_payload_env");
    let capture = root.join("payload.json");
    let command = format!(
        "test -z \"${{SQUEEZY_HOOK_PAYLOAD_FILE+x}}\" && printf '%s' \"$SQUEEZY_HOOK_PAYLOAD\" > {}",
        sh_quote_path(&capture)
    );

    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::UserPromptSubmit,
        None,
        SkillHookSpec {
            command,
            once: false,
            timeout_secs: None,
            fail_open: true,
            kind_valid: true,
            failure_policy: HookFailurePolicy::Allow,
        },
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    let results = registry.dispatch(squeezy_hooks::HookPayload::UserPromptSubmit {
        prompt: "hello".into(),
        turn_id: "1".into(),
    });

    assert!(results.iter().all(|result| result.allow), "{results:?}");
    let captured = fs::read_to_string(&capture).expect("read captured payload");
    assert!(captured.contains("\"prompt\":\"hello\""), "{captured}");
    assert!(
        !captured.contains("stale-payload-file"),
        "stale parent env leaked into hook payload: {captured}"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_large_payload_uses_file_and_removes_stale_inline_env() {
    let _guard = HOOK_ENV_LOCK.lock().expect("hook env lock");
    let _env = ScopedHookPayloadEnv::with_stale_values();
    let root = temp_workspace("skill_hook_large_payload_file");
    let payload_file_path_capture = root.join("payload-file-path");
    let copied_payload = root.join("payload-copy.json");
    let command = format!(
        "test -z \"${{SQUEEZY_HOOK_PAYLOAD+x}}\" \
         && test -n \"$SQUEEZY_HOOK_PAYLOAD_FILE\" \
         && test -f \"$SQUEEZY_HOOK_PAYLOAD_FILE\" \
         && printf '%s' \"$SQUEEZY_HOOK_PAYLOAD_FILE\" > {} \
         && cp \"$SQUEEZY_HOOK_PAYLOAD_FILE\" {}",
        sh_quote_path(&payload_file_path_capture),
        sh_quote_path(&copied_payload)
    );

    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::UserPromptSubmit,
        None,
        SkillHookSpec {
            command,
            once: false,
            timeout_secs: None,
            fail_open: true,
            kind_valid: true,
            failure_policy: HookFailurePolicy::Allow,
        },
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    let prompt = "x".repeat(PAYLOAD_INLINE_THRESHOLD + 128);
    let results = registry.dispatch(squeezy_hooks::HookPayload::UserPromptSubmit {
        prompt,
        turn_id: "1".into(),
    });

    assert!(results.iter().all(|result| result.allow), "{results:?}");
    let temp_payload_path =
        fs::read_to_string(&payload_file_path_capture).expect("read payload file path");
    assert!(
        !Path::new(temp_payload_path.trim()).exists(),
        "temporary hook payload file should be removed after dispatch"
    );
    let copied = fs::read_to_string(&copied_payload).expect("read copied payload");
    assert!(copied.len() > PAYLOAD_INLINE_THRESHOLD, "{copied}");
    assert!(
        copied.contains("\"event\":\"user_prompt_submit\""),
        "{copied}"
    );
    assert!(
        !copied.contains("stale-inline-payload"),
        "stale parent env leaked into hook payload: {copied}"
    );

    let _ = fs::remove_dir_all(root);
}
