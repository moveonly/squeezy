//! Platform-portable IPC for the shell-ask permission server.
//!
//! On Unix the transport is a tokio Unix domain socket. On Windows it is a
//! tokio named pipe. Both shapes expose the same `IpcListener` / `IpcStream`
//! interface so callers (the shell-ask server and the `squeezy ask` client)
//! can stay platform-neutral.

use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(windows)]
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

/// `ERROR_PIPE_BUSY` from Win32; surfaced when every server instance for a
/// named pipe is already serving a client. tokio re-exports this as a raw
/// `io::Error::raw_os_error` value.
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// An IPC endpoint identifier — opaque to callers other than the
/// `SQUEEZY_ASK_SOCKET` env-var plumbing. Holds a filesystem path on Unix
/// and a named-pipe identifier on Windows.
#[derive(Clone, Debug)]
pub(crate) struct IpcEndpoint {
    inner: EndpointInner,
}

#[derive(Clone, Debug)]
enum EndpointInner {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    Windows(String),
}

impl IpcEndpoint {
    /// Canonical endpoint for the shell-ask permission server.
    ///
    /// On Unix this is `<root>/.squeezy/run/shell-<sanitized_id>.sock`. On
    /// Windows it is the named pipe `\\.\pipe\squeezy-shell-<sanitized_id>-<pid>`
    /// — the PID disambiguates concurrent Squeezy processes that picked the
    /// same call_id.
    #[allow(unused_variables)]
    pub(crate) fn for_shell_ask(root: &std::path::Path, sanitized_id: &str) -> Self {
        #[cfg(unix)]
        {
            let path = root
                .join(".squeezy")
                .join("run")
                .join(format!("shell-{sanitized_id}.sock"));
            Self {
                inner: EndpointInner::Unix(path),
            }
        }
        #[cfg(windows)]
        {
            let pid = std::process::id();
            let name = format!(r"\\.\pipe\squeezy-shell-{sanitized_id}-{pid}");
            Self {
                inner: EndpointInner::Windows(name),
            }
        }
    }

    /// Unix-only short-path fallback under `/tmp` for when the preferred
    /// `.squeezy/run/` path overflows `sun_path`.
    #[cfg(unix)]
    pub(crate) fn unix_short_fallback(digest_prefix: &str) -> Self {
        Self {
            inner: EndpointInner::Unix(
                PathBuf::from("/tmp").join(format!("squeezy-{digest_prefix}.sock")),
            ),
        }
    }

    /// Recover an endpoint from the `SQUEEZY_ASK_SOCKET` value the parent
    /// process exported to a shell child.
    pub(crate) fn from_env_value(value: &OsStr) -> Self {
        #[cfg(unix)]
        {
            Self {
                inner: EndpointInner::Unix(PathBuf::from(value)),
            }
        }
        #[cfg(windows)]
        {
            Self {
                inner: EndpointInner::Windows(value.to_string_lossy().into_owned()),
            }
        }
    }

    /// Value to export as `SQUEEZY_ASK_SOCKET` when launching a child.
    pub(crate) fn as_env_value(&self) -> OsString {
        match &self.inner {
            #[cfg(unix)]
            EndpointInner::Unix(path) => path.as_os_str().to_os_string(),
            #[cfg(windows)]
            EndpointInner::Windows(name) => OsString::from(name),
        }
    }

    /// Underlying Unix path, if any. Used by callers that need to surface
    /// the path in audit/log output.
    #[cfg(unix)]
    pub(crate) fn as_unix_path(&self) -> &Path {
        match &self.inner {
            EndpointInner::Unix(path) => path.as_path(),
        }
    }
}

/// True when `err` indicates the Unix endpoint's `sun_path` exceeded the
/// platform limit. Always `false` on Windows since pipe names don't have a
/// hard length cap that surfaces this way.
pub(crate) fn is_path_too_long(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.to_string().contains("SUN_LEN") || err.raw_os_error() == Some(libc::ENAMETOOLONG)
    }
    #[cfg(not(unix))]
    {
        let _ = err;
        false
    }
}

/// Listening side of the IPC abstraction. Wraps a tokio `UnixListener` on
/// Unix and a series of `NamedPipeServer` instances on Windows.
pub(crate) struct IpcListener {
    inner: ListenerInner,
}

enum ListenerInner {
    #[cfg(unix)]
    Unix {
        listener: UnixListener,
        path: PathBuf,
    },
    #[cfg(windows)]
    Windows {
        next_server: tokio::sync::Mutex<Option<NamedPipeServer>>,
        name: String,
    },
}

impl IpcListener {
    pub(crate) fn bind(endpoint: &IpcEndpoint) -> io::Result<Self> {
        match &endpoint.inner {
            #[cfg(unix)]
            EndpointInner::Unix(path) => {
                let _ = std::fs::remove_file(path);
                let listener = UnixListener::bind(path)?;
                Ok(Self {
                    inner: ListenerInner::Unix {
                        listener,
                        path: path.clone(),
                    },
                })
            }
            #[cfg(windows)]
            EndpointInner::Windows(name) => {
                let server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(name)?;
                Ok(Self {
                    inner: ListenerInner::Windows {
                        next_server: tokio::sync::Mutex::new(Some(server)),
                        name: name.clone(),
                    },
                })
            }
        }
    }

    pub(crate) async fn accept(&self) -> io::Result<IpcStream> {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix { listener, .. } => {
                let (stream, _) = listener.accept().await?;
                Ok(IpcStream {
                    inner: StreamInner::Unix(stream),
                })
            }
            #[cfg(windows)]
            ListenerInner::Windows { next_server, name } => {
                // Take the prepared pipe instance, wait for a connection,
                // then re-arm the next instance so subsequent accepts find a
                // server already listening (matches the tokio recommended
                // pattern).
                let server = {
                    let mut guard = next_server.lock().await;
                    guard.take().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::Other, "ipc listener missing prepared server")
                    })?
                };
                server.connect().await?;
                let prepared = ServerOptions::new().create(name)?;
                {
                    let mut guard = next_server.lock().await;
                    *guard = Some(prepared);
                }
                Ok(IpcStream {
                    inner: StreamInner::WindowsServer(server),
                })
            }
        }
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix { path, .. } => {
                let _ = std::fs::remove_file(path);
            }
            #[cfg(windows)]
            ListenerInner::Windows { .. } => {}
        }
    }
}

/// One side of an established IPC connection. Implements `AsyncRead +
/// AsyncWrite + Unpin` so existing tokio code can pass it where it would
/// have passed a `UnixStream`.
pub(crate) struct IpcStream {
    inner: StreamInner,
}

enum StreamInner {
    #[cfg(unix)]
    Unix(UnixStream),
    #[cfg(windows)]
    WindowsServer(NamedPipeServer),
    #[cfg(windows)]
    WindowsClient(NamedPipeClient),
}

impl IpcStream {
    pub(crate) async fn connect(endpoint: &IpcEndpoint) -> io::Result<Self> {
        match &endpoint.inner {
            #[cfg(unix)]
            EndpointInner::Unix(path) => {
                let stream = UnixStream::connect(path).await?;
                Ok(Self {
                    inner: StreamInner::Unix(stream),
                })
            }
            #[cfg(windows)]
            EndpointInner::Windows(name) => {
                let client = loop {
                    match ClientOptions::new().open(name) {
                        Ok(client) => break client,
                        Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        Err(err) => return Err(err),
                    }
                };
                Ok(Self {
                    inner: StreamInner::WindowsClient(client),
                })
            }
        }
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = Pin::into_inner(self);
        match &mut this.inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = Pin::into_inner(self);
        match &mut this.inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = Pin::into_inner(self);
        match &mut this.inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = Pin::into_inner(self);
        match &mut this.inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}
