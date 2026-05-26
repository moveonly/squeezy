use super::*;
use std::sync::Mutex;

// env::set_var/remove_var is process-global; serialize these tests so a parallel
// runner does not let them race.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn env_check_reports_ok_when_var_set() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    // SAFETY: the lock above serializes mutations to the process env.
    unsafe {
        env::set_var("SQUEEZY_DOCTOR_TEST_KEY", "1");
    }
    let (status, detail) = env_check("SQUEEZY_DOCTOR_TEST_KEY");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_KEY");
    }
    assert_eq!(status, Status::Ok);
    assert!(detail.contains("SQUEEZY_DOCTOR_TEST_KEY"));
}

#[test]
fn env_check_warns_when_unset() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_MISSING");
    }
    let (status, _) = env_check("SQUEEZY_DOCTOR_TEST_MISSING");
    assert_eq!(status, Status::Warn);
}

#[test]
fn probe_writable_round_trips_in_tempdir() {
    let dir = std::env::temp_dir().join(format!("squeezy-doctor-probe-{}", std::process::id(),));
    let _ = fs::remove_dir_all(&dir);
    probe_writable(&dir).expect("probe");
    // probe file should have been cleaned up
    assert!(!dir.join(".squeezy-doctor-probe").exists());
    let _ = fs::remove_dir_all(&dir);
}
