use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use crate::ipc;
use crate::ipc::IpcListener;
use crate::{
    IpcEndpoint, IpcStream, ShellAskApprover, ShellAskDecision, ShellAskRequest, sha256_hex,
};

pub(crate) struct ShellAskServer {
    endpoint: IpcEndpoint,
    task: tokio::task::JoinHandle<()>,
}

impl ShellAskServer {
    pub(crate) async fn start(
        root: &Path,
        call_id: &str,
        parent_command: &str,
        workdir: &Path,
        approver: ShellAskApprover,
        cancel: CancellationToken,
    ) -> std::io::Result<Self> {
        let sanitized = sanitize_shell_call_id(call_id);
        #[cfg(unix)]
        {
            let run_dir = root.join(".squeezy").join("run");
            fs::create_dir_all(&run_dir)?;
        }
        let primary = IpcEndpoint::for_shell_ask(root, &sanitized);
        let (endpoint, listener) = match IpcListener::bind(&primary) {
            Ok(listener) => (primary, listener),
            #[cfg(unix)]
            Err(err) if ipc::is_path_too_long(&err) => {
                let digest = sha256_hex(format!("{}:{call_id}", root.display()));
                let fallback = IpcEndpoint::unix_short_fallback(&digest[..16]);
                let listener = IpcListener::bind(&fallback)?;
                (fallback, listener)
            }
            Err(err) => return Err(err),
        };
        let call_id = call_id.to_string();
        let parent_command = parent_command.to_string();
        let workdir = workdir.to_path_buf();
        let task = tokio::spawn(async move {
            shell_ask_server_loop(listener, call_id, parent_command, workdir, approver, cancel)
                .await;
        });
        Ok(Self { endpoint, task })
    }

    pub(crate) fn env_value(&self) -> std::ffi::OsString {
        self.endpoint.as_env_value()
    }
}

impl Drop for ShellAskServer {
    fn drop(&mut self) {
        self.task.abort();
        self.endpoint.remove_local_artifacts();
    }
}

#[derive(Debug, Deserialize)]
struct ShellAskWireRequest {
    command: String,
    justification: String,
}

async fn shell_ask_server_loop(
    listener: IpcListener,
    call_id: String,
    parent_command: String,
    workdir: PathBuf,
    approver: ShellAskApprover,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok(stream) = accepted else {
            break;
        };
        let request_call_id = call_id.clone();
        let request_parent = parent_command.clone();
        let request_workdir = workdir.clone();
        let request_approver = approver.clone();
        tokio::spawn(async move {
            let _ = handle_shell_ask_client(
                stream,
                request_call_id,
                request_parent,
                request_workdir,
                request_approver,
            )
            .await;
        });
    }
}

async fn handle_shell_ask_client(
    mut stream: IpcStream,
    call_id: String,
    parent_command: String,
    workdir: PathBuf,
    approver: ShellAskApprover,
) -> std::io::Result<()> {
    const MAX_ASK_REQUEST_BYTES: usize = 16 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > MAX_ASK_REQUEST_BYTES {
            let response = ShellAskDecision::deny("in-flight permission request is too large");
            stream
                .write_all(&serde_json::to_vec(&response).map_err(std::io::Error::other)?)
                .await?;
            stream.shutdown().await?;
            return Ok(());
        }
    }

    let decision = match serde_json::from_slice::<ShellAskWireRequest>(&bytes) {
        Ok(wire) if !wire.command.trim().is_empty() => {
            approver(ShellAskRequest {
                call_id,
                parent_command,
                command: wire.command,
                justification: wire.justification,
                workdir,
            })
            .await
        }
        Ok(_) => ShellAskDecision::deny("in-flight permission command must not be empty"),
        Err(err) => ShellAskDecision::deny(format!("invalid in-flight permission request: {err}")),
    };
    stream
        .write_all(&serde_json::to_vec(&decision).map_err(std::io::Error::other)?)
        .await?;
    stream.shutdown().await?;
    Ok(())
}

fn sanitize_shell_call_id(call_id: &str) -> String {
    let mut out = String::new();
    for ch in call_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "call".to_string()
    } else {
        out
    }
}
