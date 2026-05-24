use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::{Result, SqueezyError};

pub(crate) fn increment(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_string()).or_default() += 1;
}

pub(crate) fn temp_dir(prefix: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| SqueezyError::Graph(format!("clock error: {err}")))?
        .as_nanos();
    let path = env::temp_dir().join(format!("{prefix}-{nonce}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect::<String>()
}

pub(crate) struct DeterministicRng {
    pub(crate) state: u64,
}

impl DeterministicRng {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_usize(&mut self, upper_bound: usize) -> usize {
        if upper_bound == 0 {
            return 0;
        }
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.state as usize) % upper_bound
    }
}
