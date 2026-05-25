use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::driver::EvalError;

/// One "frame" per completed (or terminated) agent turn. This is the
/// human-friendly view of what a TUI user would have seen: the assembled
/// assistant text, plus the tool calls fired and any error/finish reason.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameRecord {
    pub turn_id: String,
    pub prompt: String,
    /// Concatenation of all assistant text deltas for this turn, in order.
    pub assistant_text: String,
    pub tool_calls: Vec<String>,
    pub tool_errors: Vec<String>,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub finish: FrameFinish,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFinish {
    #[default]
    Completed,
    Cancelled,
    Failed,
    NoTurn,
}

pub struct FrameWriter {
    inner: Mutex<FrameInner>,
}

struct FrameInner {
    path: PathBuf,
    file: std::fs::File,
}

impl FrameWriter {
    pub fn create(dir: &Path) -> Result<Self, EvalError> {
        std::fs::create_dir_all(dir)
            .map_err(|err| EvalError::Io(format!("create_dir_all {dir:?}: {err}")))?;
        let path = dir.join("frames.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
        Ok(Self {
            inner: Mutex::new(FrameInner { path, file }),
        })
    }

    pub fn write(&self, frame: &FrameRecord) -> Result<(), EvalError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| EvalError::Internal(format!("frame mutex poisoned: {err}")))?;
        let line = serde_json::to_string(frame)
            .map_err(|err| EvalError::Internal(format!("serialize frame: {err}")))?;
        writeln!(guard.file, "{line}")
            .map_err(|err| EvalError::Io(format!("append frame: {err}")))?;
        Ok(())
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().expect("frame lock").path.clone()
    }
}
