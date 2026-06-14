use std::env;
use std::process::{Command, Stdio};

/// Best-effort browser launcher. Each platform has its own CLI:
/// `open` on macOS, `xdg-open` on Linux, `cmd /C start` on Windows.
/// Falls back to an explicit error so the caller can print the URL
/// for the user to open manually.
///
/// On Linux this delegates to [`try_open_browser`] so both code paths
/// share the same fallback ladder (xdg-open -> gio open ->
/// sensible-browser), stdio suppression, and exit-status check.
/// The old single-launcher path silently returned `Ok(())` even when
/// `xdg-open` exited with a non-zero status, which produced a
/// misleading "Browser launched" message on headless/minimal systems.
pub(super) fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).status()?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        if try_open_browser(url) {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no working browser launcher found (tried xdg-open, gio open, sensible-browser)",
            ))
        }
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no browser launcher for this platform",
        ))
    }
}

/// Returns `true` on Linux when no display or browser environment is
/// present — a reliable signal that the process is running in an SSH
/// session, container, CI runner, or other headless environment where
/// launching a desktop browser will silently fail.
///
/// Checks `$DISPLAY` (X11), `$WAYLAND_DISPLAY`, and `$BROWSER`
/// (explicit override).  Always returns `false` on non-Linux platforms.
pub(crate) fn is_headless_linux() -> bool {
    #[cfg(target_os = "linux")]
    {
        env::var_os("DISPLAY").is_none()
            && env::var_os("WAYLAND_DISPLAY").is_none()
            && env::var_os("BROWSER").is_none()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Best-effort browser launch. Returns `false` when no launcher
/// succeeded — the caller falls back to printing the URL for manual
/// copy-paste. Honors `SQUEEZY_OAUTH_BROWSER` for headless tests so
/// they can confirm the URL was emitted without spawning a process.
pub(super) fn try_open_browser(url: &str) -> bool {
    if let Ok(override_cmd) = env::var("SQUEEZY_OAUTH_BROWSER")
        && override_cmd.trim() == "0"
    {
        return false;
    }
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        // `cmd /c start "" <url>` is the canonical Windows shell launcher;
        // the empty title string keeps `start` from swallowing the URL as
        // the window title.
        &[("cmd", &["/c", "start", ""])]
    } else {
        // Most Linux desktops + BSDs ship xdg-open; fall back to a couple
        // of common alternates so an unusual installation still works.
        &[
            ("xdg-open", &[]),
            ("gio", &["open"]),
            ("sensible-browser", &[]),
        ]
    };
    for (cmd, extra_args) in candidates {
        let mut command = Command::new(cmd);
        for arg in *extra_args {
            command.arg(arg);
        }
        command.arg(url);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        if command.status().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}
