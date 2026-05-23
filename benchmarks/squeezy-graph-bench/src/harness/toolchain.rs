use std::{path::PathBuf, process::Command};

pub fn which(program: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(program).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    Some(PathBuf::from(path.trim()))
}
