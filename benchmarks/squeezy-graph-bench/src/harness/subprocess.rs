use std::{ffi::OsStr, process::Command, time::Instant};

use squeezy_core::{Result, SqueezyError};

pub struct TimedOutput {
    pub elapsed_ms: u128,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
}

pub fn run_timed<I, S>(program: &str, args: I) -> Result<TimedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let started = Instant::now();
    let output = Command::new(program).args(args).output().map_err(|err| {
        SqueezyError::Graph(format!("failed to run benchmark subprocess {program}: {err}"))
    })?;
    Ok(TimedOutput {
        elapsed_ms: started.elapsed().as_millis(),
        stdout: output.stdout,
        stderr: output.stderr,
        success: output.status.success(),
    })
}
