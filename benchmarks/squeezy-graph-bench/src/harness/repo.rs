use std::{path::Path, process::Command};

use squeezy_core::{Result, SqueezyError};

pub fn clone_shallow(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--depth", "1", "--filter=blob:none", url])
        .arg(destination)
        .status()
        .map_err(|err| SqueezyError::Graph(format!("failed to spawn git clone: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(SqueezyError::Graph(format!(
            "git clone {url} failed with {status}"
        )))
    }
}
