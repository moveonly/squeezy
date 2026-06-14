use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{Terminal, backend::TestBackend, text::Line};
use squeezy_agent::Agent;
use squeezy_core::{AppConfig, PermissionMode, PermissionPolicy, SessionMode};
use squeezy_llm::UnavailableProvider;

use crate::{Clipboard, TuiApp, render};

pub(crate) fn test_app(mode: SessionMode) -> TuiApp {
    test_app_with_clipboard(mode, Box::new(NoopClipboard))
}

pub(crate) fn test_app_with_clipboard(mode: SessionMode, clipboard: Box<dyn Clipboard>) -> TuiApp {
    let config = test_config(mode);
    // The OSC 52 capability is now forced on at the `build_clipboard_chain`
    // funnel under `cfg(test)`, so every app built here (and every direct
    // `new_with_clipboard` caller) gets a host-independent OSC 52-capable chain
    // and `deliver_copy` takes the observable `app.clipboard` fast path. Tests
    // that model an OSC 52-ignoring terminal override it with
    // `set_clipboard_chain_for_test` after construction.
    TuiApp::new_with_clipboard("scripted", &config, mode, None, clipboard)
}

pub(crate) fn test_app_with_config(config: &AppConfig, mode: SessionMode) -> TuiApp {
    TuiApp::new_with_clipboard("scripted", config, mode, None, Box::new(NoopClipboard))
}

pub(crate) fn test_config(mode: SessionMode) -> AppConfig {
    AppConfig {
        model: "gpt-test".to_string(),
        session_mode: mode,
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            edit: PermissionMode::Ask,
            shell: PermissionMode::Ask,
            web: PermissionMode::Ask,
            ..Default::default()
        },
        config_sources: vec!["defaults".to_string()],
        // See `test_agent`: keep the test fixture off the real workspace so
        // `Agent::new` / `TuiApp::new` don't crawl the repo on every test.
        workspace_root: temp_workspace("config"),
        ..Default::default()
    }
}

pub(crate) fn test_config_with_root(mode: SessionMode, root: PathBuf) -> AppConfig {
    AppConfig {
        workspace_root: root,
        ..test_config(mode)
    }
}

pub(crate) fn test_agent(mode: SessionMode) -> Agent {
    // Use a fresh empty temp workspace so the agent's tool registry doesn't
    // crawl the entire repo (which adds seconds per test, especially on
    // Windows where filesystem syscalls are slow). The TUI tests never
    // touch the workspace; they only need a valid `AppConfig`.
    test_agent_with_config(AppConfig {
        session_mode: mode,
        workspace_root: temp_workspace("agent"),
        ..Default::default()
    })
}

pub(crate) fn test_agent_with_config(config: AppConfig) -> Agent {
    Agent::new(
        config,
        Arc::new(UnavailableProvider::new("scripted", "test provider")),
    )
}

pub(crate) fn test_agent_without_session_log(mode: SessionMode) -> Agent {
    test_agent_without_session_log_with_config(AppConfig {
        session_mode: mode,
        workspace_root: temp_workspace("agent"),
        ..Default::default()
    })
}

pub(crate) fn test_agent_without_session_log_with_config(config: AppConfig) -> Agent {
    Agent::new_ephemeral(
        config,
        Arc::new(UnavailableProvider::new("scripted", "test provider")),
    )
}

pub(crate) fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_tui_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

pub(crate) fn render_to_string(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| render(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

pub(crate) fn lines_to_plain_text(lines: &[Line<'_>]) -> String {
    let mut output = String::new();
    for line in lines {
        output.push_str(&rendered_line_text(line));
        output.push('\n');
    }
    output
}

pub(crate) fn rendered_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

pub(crate) struct NoopClipboard;

impl Clipboard for NoopClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

pub(crate) struct RecordingClipboard {
    pub(crate) writes: Arc<std::sync::Mutex<Vec<String>>>,
    pub(crate) error: Option<String>,
}

impl Clipboard for RecordingClipboard {
    fn copy_text(&mut self, text: &str) -> std::result::Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.writes.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

/// Clipboard double whose `read_text` / `read_image` return canned values, for
/// exercising the in-app paste chord without touching the real system clipboard.
#[derive(Default)]
pub(crate) struct ReadableClipboard {
    pub(crate) read: Option<String>,
    pub(crate) image: Option<(Vec<u8>, String)>,
}

impl Clipboard for ReadableClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }

    fn read_text(&mut self) -> Option<String> {
        self.read.clone()
    }

    fn read_image(&mut self) -> Option<(Vec<u8>, String)> {
        self.image.clone()
    }
}
