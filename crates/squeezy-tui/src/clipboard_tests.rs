use super::*;
use std::collections::HashMap;
use std::ffi::OsString;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an env-getter closure backed by a fixture map, mirroring the
/// DEC-2026 detection tests.
fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |key: &str| map.get(key).map(OsString::from)
}

/// Decode the bytes a single OSC 52 `write_terminal` call carries back to the
/// raw base64 payload (strip `ESC ] 52 ; c ;` prefix and `BEL`/`ST` suffix).
fn osc52_b64_from_terminal_calls(calls: &[SinkCall]) -> String {
    let mut acc = Vec::new();
    for call in calls {
        if let SinkCall::Terminal { bytes } = call {
            acc.extend_from_slice(bytes);
        }
    }
    let s = String::from_utf8(acc).expect("OSC52 bytes are ascii");
    // Strip the single-shot form: ESC ] 52 ; c ; <b64> BEL
    let s = s
        .strip_prefix("\x1b]52;c;")
        .map(|rest| rest.trim_end_matches('\x07').to_string())
        .unwrap_or(s);
    // Strip chunked terminator if present.
    s.trim_end_matches("\x1b\\").to_string()
}

const PLATFORM_OK: SinkScript = SinkScript {
    terminal_error: None,
    command_outcome: CommandScript::Success,
    temp_file_error: None,
    temp_dir: PathBuf::new(), // replaced below where needed
};

fn osc52_provider_list(extra: &[PlatformCommand]) -> Vec<ClipboardProvider> {
    let mut v = vec![ClipboardProvider::Osc52];
    for cmd in extra {
        v.push(ClipboardProvider::PlatformCommand(*cmd));
    }
    v.push(ClipboardProvider::TempFile);
    v
}

const PBCOPY: PlatformCommand = PlatformCommand {
    program: "pbcopy",
    args: &[],
};

// ---------------------------------------------------------------------------
// 1. OSC 52 selected under the limit
// ---------------------------------------------------------------------------

#[test]
fn osc52_selected_under_limit_single_write_exact_bytes() {
    let sink = RecordingSink::new();
    let calls = sink.handle();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));

    let req = CopyRequest::new("hello", "assistant message");
    let outcome = chain.copy(&req);

    assert_eq!(
        outcome,
        CopyOutcome::Copied {
            provider: ClipboardProviderKind::Osc52,
            lines: 1,
            bytes: 5,
        }
    );

    let recorded = calls.lock().unwrap().clone();
    // Exactly one terminal write, no command spawn, no temp file.
    assert_eq!(recorded.len(), 1, "expected exactly one sink call");
    match &recorded[0] {
        SinkCall::Terminal { bytes } => {
            let expected = format!("\x1b]52;c;{}\x07", crate::base64_encode(b"hello"));
            assert_eq!(bytes, expected.as_bytes());
            // And the base64 round-trips.
            assert_eq!(
                osc52_b64_from_terminal_calls(&recorded),
                crate::base64_encode(b"hello")
            );
        }
        other => panic!("expected Terminal call, got {other:?}"),
    }
}

#[test]
fn osc52_payload_exactly_at_limit_still_single_write() {
    // Choose a payload whose base64 length equals the cap precisely.
    let cap = 8usize; // base64 of 6 bytes -> 8 chars
    let sink = RecordingSink::new();
    let calls = sink.handle();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    chain.set_osc52_max_bytes(cap);

    let req = CopyRequest::new("abcdef", "x"); // 6 bytes -> 8 b64 chars == cap
    let outcome = chain.copy(&req);
    assert!(matches!(
        outcome,
        CopyOutcome::Copied {
            provider: ClipboardProviderKind::Osc52,
            ..
        }
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// 2. Over the limit: never a wire-level "chunk" no-op that claims success
// ---------------------------------------------------------------------------

/// An oversized OSC 52 payload must NEVER claim success via the terminal, even
/// with the (now inert) chunk toggle ON: a wire-level split still emits one
/// escape the terminal drops, so the chain must fall through to the platform
/// command. With a `[Osc52, PlatformCommand]` chain the outcome is `Copied` via
/// `Platform`, NOT via `Osc52`.
#[test]
fn osc52_over_cap_with_chunk_falls_through_to_platform_not_osc52() {
    let sink = RecordingSink::new(); // terminal write succeeds by default
    let mut chain = ClipboardChain::with_providers(
        sink,
        vec![
            ClipboardProvider::Osc52,
            ClipboardProvider::PlatformCommand(PBCOPY),
        ],
    );
    chain.set_osc52_max_bytes(4).set_osc52_chunk(true); // chunk ON, payload blows past cap

    let req = CopyRequest::new("the quick brown fox jumps", "x");
    let outcome = chain.copy(&req);

    // Falls through to the platform provider, not a false Osc52 "success".
    assert!(
        matches!(
            outcome,
            CopyOutcome::Copied {
                provider: ClipboardProviderKind::Platform("pbcopy"),
                ..
            }
        ),
        "over-cap chunked OSC52 must fall through to platform, got {outcome:?}"
    );
}

/// With an OSC-52-only chain and the chunk toggle ON, an over-cap payload yields
/// `Failed` (no provider left), never a false `Copied`.
#[test]
fn osc52_over_cap_with_chunk_only_chain_yields_failed_not_copied() {
    let sink = RecordingSink::new();
    let mut chain = ClipboardChain::with_providers(sink, vec![ClipboardProvider::Osc52]);
    chain.set_osc52_max_bytes(4).set_osc52_chunk(true);

    let req = CopyRequest::new("the quick brown fox jumps", "x");
    let outcome = chain.copy(&req);

    assert!(
        matches!(outcome, CopyOutcome::Failed { .. }),
        "over-cap chunked OSC52-only chain must fail, got {outcome:?}"
    );
}

#[test]
fn osc52_over_limit_without_chunking_falls_through_to_platform() {
    let sink = RecordingSink::new();
    let calls = sink.handle();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    chain.set_osc52_max_bytes(4); // chunking off (default)

    let req = CopyRequest::new("a much longer payload than four base64 chars", "x");
    let outcome = chain.copy(&req);

    assert!(
        matches!(
            outcome,
            CopyOutcome::Copied {
                provider: ClipboardProviderKind::Platform("pbcopy"),
                ..
            }
        ),
        "expected platform fallback, got {outcome:?}"
    );

    let recorded = calls.lock().unwrap().clone();
    // No terminal write was attempted (OSC52 refused up front), one command.
    assert!(
        !recorded
            .iter()
            .any(|c| matches!(c, SinkCall::Terminal { .. })),
        "over-limit OSC52 without chunking must not write to terminal"
    );
    match recorded
        .iter()
        .find(|c| matches!(c, SinkCall::Command { .. }))
    {
        Some(SinkCall::Command {
            program, payload, ..
        }) => {
            assert_eq!(program, "pbcopy");
            assert_eq!(payload, b"a much longer payload than four base64 chars");
        }
        _ => panic!("expected a Command call, got {recorded:?}"),
    }
}

/// Phase 9 platform-hardening (tmux/SSH): at the PRODUCTION default cap
/// (`DEFAULT_OSC52_MAX_BYTES` = 8 KiB, chunking OFF — the config
/// `build_clipboard_chain` ships), an oversized selection must NOT emit a
/// truncated or oversized OSC 52 escape. It degrades gracefully to the next
/// provider in the chain (here `pbcopy`), so a payload past what the outer
/// terminal/tmux `set-clipboard` buffer accepts never silently corrupts the
/// clipboard or the screen. A payload just UNDER the cap still goes via OSC 52.
#[test]
fn osc52_default_cap_falls_through_when_oversized_otherwise_uses_terminal() {
    // 1. Just under the cap: OSC 52 is used. base64 inflates by 4/3, so a raw
    //    payload of ~half the cap is comfortably under the 8 KiB base64 limit.
    let under = "u".repeat(DEFAULT_OSC52_MAX_BYTES / 2);
    let sink = RecordingSink::new();
    let calls = sink.handle();
    // Default cap, chunking off — exactly the production `with_providers` config.
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    let outcome = chain.copy(&CopyRequest::new(&under, "x"));
    assert!(
        matches!(
            outcome,
            CopyOutcome::Copied {
                provider: ClipboardProviderKind::Osc52,
                ..
            }
        ),
        "a payload under the 8 KiB cap must go via OSC 52, got {outcome:?}"
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c, SinkCall::Terminal { .. })),
        "under-cap OSC 52 must write the escape to the terminal"
    );

    // 2. Over the cap: must fall through to the platform command and emit NO
    //    terminal escape (no truncation, no oversized sequence on the wire).
    let over = "o".repeat(DEFAULT_OSC52_MAX_BYTES * 2);
    let sink = RecordingSink::new();
    let calls = sink.handle();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    let outcome = chain.copy(&CopyRequest::new(&over, "x"));
    assert!(
        matches!(
            outcome,
            CopyOutcome::Copied {
                provider: ClipboardProviderKind::Platform("pbcopy"),
                ..
            }
        ),
        "an over-cap payload must degrade to the platform provider, got {outcome:?}"
    );
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c, SinkCall::Terminal { .. })),
        "over-cap OSC 52 must never write a (truncated/oversized) escape to the terminal"
    );
}

// ---------------------------------------------------------------------------
// 3. Platform fallback when OSC 52 is unsupported
// ---------------------------------------------------------------------------

#[test]
fn platform_selected_when_osc52_absent_from_chain() {
    // Capability probe says no OSC52 -> default_chain omits it.
    let sink = RecordingSink::new();
    let calls = sink.handle();
    let caps = ClipboardCapabilities {
        osc52: false,
        ..Default::default()
    };
    let mut chain = ClipboardChain::default_chain(sink, caps, vec![PBCOPY]);

    let req = CopyRequest::new("clipboard please", "transcript");
    let outcome = chain.copy(&req);

    assert!(matches!(
        outcome,
        CopyOutcome::Copied {
            provider: ClipboardProviderKind::Platform("pbcopy"),
            ..
        }
    ));

    let recorded = calls.lock().unwrap().clone();
    assert!(
        !recorded
            .iter()
            .any(|c| matches!(c, SinkCall::Terminal { .. })),
        "OSC52 must not be attempted when the chain omits it"
    );
    assert_eq!(recorded.len(), 1, "exactly the platform command runs");
}

#[test]
fn platform_candidates_tried_in_order_until_one_succeeds() {
    // First candidate spawn-fails, second succeeds. Drive this with two
    // explicit providers and a script where the command "succeeds" — to test
    // ordering we instead use a spawn error on the whole sink and a temp-file
    // success, asserting both commands were tried.
    let script = SinkScript {
        command_outcome: CommandScript::SpawnError("not found".to_string()),
        temp_dir: PathBuf::from("/tmp/squeezy-test"),
        ..Default::default()
    };
    let sink = RecordingSink::with_script(script);
    let calls = sink.handle();
    let cmd_a = PlatformCommand {
        program: "xclip",
        args: &["-selection", "clipboard"],
    };
    let cmd_b = PlatformCommand {
        program: "xsel",
        args: &["--clipboard", "--input"],
    };
    let providers = vec![
        ClipboardProvider::PlatformCommand(cmd_a),
        ClipboardProvider::PlatformCommand(cmd_b),
        ClipboardProvider::TempFile,
    ];
    let mut chain = ClipboardChain::with_providers(sink, providers);

    let req = CopyRequest::new("x", "y");
    let outcome = chain.copy(&req);

    // Both spawn-fail -> temp-file fallback.
    assert!(matches!(outcome, CopyOutcome::WroteTempFile { .. }));

    let recorded = calls.lock().unwrap().clone();
    let programs: Vec<_> = recorded
        .iter()
        .filter_map(|c| match c {
            SinkCall::Command { program, .. } => Some(program.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(programs, vec!["xclip".to_string(), "xsel".to_string()]);
}

#[test]
fn platform_nonzero_exit_falls_through_with_stderr_in_reason() {
    let script = SinkScript {
        command_outcome: CommandScript::Exit {
            code: 1,
            stderr: "no display".to_string(),
        },
        temp_file_error: Some("disk full".to_string()),
        ..Default::default()
    };
    let sink = RecordingSink::with_script(script);
    let mut chain = ClipboardChain::with_providers(
        sink,
        vec![
            ClipboardProvider::PlatformCommand(PBCOPY),
            ClipboardProvider::TempFile,
        ],
    );

    let req = CopyRequest::new("x", "y");
    let outcome = chain.copy(&req);

    match outcome {
        CopyOutcome::Failed { reason } => {
            // The last provider tried was temp-file, so its reason wins.
            assert!(reason.contains("disk full"), "reason was: {reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 4. Temp-file fallback when OSC 52 and platform both fail
// ---------------------------------------------------------------------------

#[test]
fn temp_file_fallback_when_osc52_and_platform_fail() {
    let script = SinkScript {
        command_outcome: CommandScript::SpawnError("missing binary".to_string()),
        temp_dir: PathBuf::from("/tmp/squeezy-test"),
        ..Default::default()
    };
    let sink = RecordingSink::with_script(script);
    let calls = sink.handle();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    chain.set_osc52_max_bytes(2); // force OSC52 to refuse (no chunk)

    let req = CopyRequest::new("payload", "assistant message");
    let outcome = chain.copy(&req);

    match outcome {
        CopyOutcome::WroteTempFile { path, bytes } => {
            assert_eq!(bytes, 7);
            // Path roots at the scripted temp dir with the sanitized label.
            assert!(path.starts_with("/tmp/squeezy-test"), "path was {path:?}");
            assert!(
                path.to_string_lossy().ends_with("assistant-message.txt"),
                "path was {path:?}"
            );
        }
        other => panic!("expected WroteTempFile, got {other:?}"),
    }

    let recorded = calls.lock().unwrap().clone();
    // OSC52 refused up front (no terminal write); command spawn-failed; then
    // temp-file. The temp-file payload must be the raw bytes.
    assert!(matches!(
        recorded.last().unwrap(),
        SinkCall::TempFile { .. }
    ));
    if let SinkCall::TempFile { payload, .. } = recorded.last().unwrap() {
        assert_eq!(payload, b"payload");
    }
}

// ---------------------------------------------------------------------------
// 5. Failure status when everything fails
// ---------------------------------------------------------------------------

#[test]
fn all_providers_fail_yields_failed_with_last_reason() {
    let script = SinkScript {
        terminal_error: Some("tty gone".to_string()),
        command_outcome: CommandScript::SpawnError("nope".to_string()),
        temp_file_error: Some("read-only fs".to_string()),
        ..Default::default()
    };
    let sink = RecordingSink::with_script(script);
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));

    let req = CopyRequest::new("x", "y");
    let outcome = chain.copy(&req);

    match outcome {
        CopyOutcome::Failed { reason } => {
            assert!(reason.contains("read-only fs"), "reason was: {reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // status/toast reflect the failure.
    assert!(
        chain
            .copy(&req)
            .status_message()
            .starts_with("copy failed:")
    );
    let (_, variant) = chain.copy(&req).toast();
    assert_eq!(variant, ToastVariant::Error);
}

// ---------------------------------------------------------------------------
// Confirmation gate (privacy control)
// ---------------------------------------------------------------------------

#[test]
fn confirm_threshold_exceeded_without_confirmation_attempts_no_provider() {
    let sink = RecordingSink::new();
    let calls = sink.handle();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    chain.set_confirm_threshold(Some(3));

    let req = CopyRequest::new("123456", "secret"); // 6 bytes > 3, unconfirmed
    let outcome = chain.copy(&req);

    assert_eq!(outcome, CopyOutcome::NeedsConfirmation { bytes: 6 });
    assert!(
        calls.lock().unwrap().is_empty(),
        "no sink calls allowed before confirmation"
    );
}

#[test]
fn confirm_threshold_satisfied_when_confirmed() {
    let sink = RecordingSink::new();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    chain.set_confirm_threshold(Some(3));

    let mut req = CopyRequest::new("123456", "secret");
    req.confirmed = true;
    let outcome = chain.copy(&req);
    assert!(matches!(
        outcome,
        CopyOutcome::Copied {
            provider: ClipboardProviderKind::Osc52,
            ..
        }
    ));
}

#[test]
fn confirm_threshold_not_exceeded_proceeds_without_confirmation() {
    let sink = RecordingSink::new();
    let mut chain = ClipboardChain::with_providers(sink, osc52_provider_list(&[PBCOPY]));
    chain.set_confirm_threshold(Some(100));

    let req = CopyRequest::new("small", "x"); // 5 bytes < 100
    let outcome = chain.copy(&req);
    assert!(matches!(outcome, CopyOutcome::Copied { .. }));
}

// ---------------------------------------------------------------------------
// status_message / toast mapping for each outcome
// ---------------------------------------------------------------------------

#[test]
fn status_and_toast_mapping_for_each_outcome() {
    let copied = CopyOutcome::Copied {
        provider: ClipboardProviderKind::Osc52,
        lines: 3,
        bytes: 10,
    };
    assert_eq!(copied.status_message(), "copied 3 lines");
    assert_eq!(copied.toast().1, ToastVariant::Success);

    let copied_one = CopyOutcome::Copied {
        provider: ClipboardProviderKind::Platform("pbcopy"),
        lines: 1,
        bytes: 4,
    };
    assert_eq!(copied_one.status_message(), "copied 1 line");

    let temp = CopyOutcome::WroteTempFile {
        path: PathBuf::from("/tmp/squeezy-copy-x.txt"),
        bytes: 9,
    };
    assert_eq!(temp.status_message(), "wrote /tmp/squeezy-copy-x.txt");
    assert_eq!(temp.toast().1, ToastVariant::Warning);

    let needs = CopyOutcome::NeedsConfirmation { bytes: 42 };
    assert_eq!(needs.toast().1, ToastVariant::Info);

    let failed = CopyOutcome::Failed {
        reason: "boom".to_string(),
    };
    assert_eq!(failed.status_message(), "copy failed: boom");
    assert_eq!(failed.toast().1, ToastVariant::Error);
}

// ---------------------------------------------------------------------------
// Capability probe truth table (parity with DEC-2026 detection tests)
// ---------------------------------------------------------------------------

#[test]
fn capability_probe_detects_known_osc52_terminals() {
    for (k, v) in [
        ("KITTY_WINDOW_ID", "1"),
        ("WEZTERM_PANE", "0"),
        ("GHOSTTY_RESOURCES_DIR", "/x"),
        ("ITERM_SESSION_ID", "w0t0p0"),
        ("TMUX", "/tmp/tmux-1000/default,123,0"),
    ] {
        let caps = detect_clipboard_capabilities_from_env(env_map(&[(k, v)]));
        assert!(caps.osc52, "expected osc52=true for {k}");
    }
}

#[test]
fn capability_probe_detects_term_program_and_term() {
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("TERM_PROGRAM", "iTerm.app")]));
    assert!(caps.osc52);
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("TERM_PROGRAM", "vscode")]));
    assert!(caps.osc52);
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("TERM", "xterm-kitty")]));
    assert!(caps.osc52);
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("TERM", "screen-256color")]));
    assert!(caps.osc52);
}

#[test]
fn capability_probe_false_for_unknown_terminal() {
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("TERM", "dumb")]));
    assert!(!caps.osc52);
    let caps = detect_clipboard_capabilities_from_env(env_map(&[]));
    assert!(!caps.osc52);
}

#[test]
fn capability_probe_detects_ssh_session() {
    // Either standard SSH marker flips `ssh` on.
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("SSH_TTY", "/dev/pts/3")]));
    assert!(caps.ssh, "SSH_TTY marks a remote session");
    let caps = detect_clipboard_capabilities_from_env(env_map(&[(
        "SSH_CONNECTION",
        "10.0.0.1 5000 10.0.0.2 22",
    )]));
    assert!(caps.ssh, "SSH_CONNECTION marks a remote session");
    // No SSH markers -> local session.
    let caps = detect_clipboard_capabilities_from_env(env_map(&[("TERM_PROGRAM", "iTerm.app")]));
    assert!(!caps.ssh, "no SSH markers -> local session");
}

// ---------------------------------------------------------------------------
// Platform command selection
// ---------------------------------------------------------------------------

#[test]
fn platform_commands_are_nonempty_and_consult_env_on_linux() {
    // macOS/Windows ignore env; Linux consults WAYLAND_DISPLAY. In all cases
    // the candidate list is non-empty so the chain always has something to
    // try before temp-file.
    let with_wayland = platform_commands(env_map(&[("WAYLAND_DISPLAY", "wayland-0")]));
    assert!(!with_wayland.is_empty());
    let without = platform_commands(env_map(&[]));
    assert!(!without.is_empty());

    #[cfg(target_os = "macos")]
    assert_eq!(without[0].program, "pbcopy");

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Wayland present -> wl-copy is preferred (first).
        assert_eq!(with_wayland[0].program, "wl-copy");
        // Without it, X11 helpers lead.
        assert_eq!(without[0].program, "xclip");
    }
}

#[test]
fn platform_read_commands_are_nonempty_and_consult_env_on_linux() {
    // The paste side mirrors the copy side: macOS/Windows ignore env, Linux
    // consults WAYLAND_DISPLAY, and the list is always non-empty.
    let with_wayland = platform_read_commands(env_map(&[("WAYLAND_DISPLAY", "wayland-0")]));
    assert!(!with_wayland.is_empty());
    let without = platform_read_commands(env_map(&[]));
    assert!(!without.is_empty());

    #[cfg(target_os = "macos")]
    assert_eq!(without[0].program, "pbpaste");

    #[cfg(target_os = "windows")]
    assert_eq!(without[0].program, "powershell");

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Wayland present -> wl-paste is preferred (first).
        assert_eq!(with_wayland[0].program, "wl-paste");
        // Without it, the X11 reader leads.
        assert_eq!(without[0].program, "xclip");
    }
}

// ---------------------------------------------------------------------------
// default_chain ordering
// ---------------------------------------------------------------------------

/// REMOTE/SSH session: OSC 52 leads (it is the only sink that reaches the
/// user's *local* clipboard), then the platform command, then temp-file. The
/// fast OSC 52-only path is trusted here (`prefers_osc52()` is true).
#[test]
fn default_chain_ssh_orders_osc52_then_platform_then_tempfile() {
    let sink = RecordingSink::new();
    let chain = ClipboardChain::default_chain(
        sink,
        ClipboardCapabilities {
            osc52: true,
            ssh: true,
        },
        vec![PBCOPY],
    );
    assert_eq!(
        chain.providers,
        vec![
            ClipboardProvider::Osc52,
            ClipboardProvider::PlatformCommand(PBCOPY),
            ClipboardProvider::TempFile,
        ]
    );
    assert!(
        chain.prefers_osc52(),
        "over SSH, OSC 52 is the preferred provider"
    );
}

/// LOCAL session WITH a platform command: the verifiable platform command leads
/// so the copy actually lands even on OSC 52-ignoring terminals; OSC 52 trails
/// as a fallback, then temp-file. `prefers_osc52()` is false, so `deliver_copy`
/// routes through the chain instead of taking the unobservable OSC 52 fast path.
#[test]
fn default_chain_local_orders_platform_then_osc52_then_tempfile() {
    let sink = RecordingSink::new();
    let chain = ClipboardChain::default_chain(
        sink,
        ClipboardCapabilities {
            osc52: true,
            ssh: false,
        },
        vec![PBCOPY],
    );
    assert_eq!(
        chain.providers,
        vec![
            ClipboardProvider::PlatformCommand(PBCOPY),
            ClipboardProvider::Osc52,
            ClipboardProvider::TempFile,
        ]
    );
    assert!(
        !chain.prefers_osc52(),
        "locally, the verifiable platform command is preferred over OSC 52"
    );
    assert!(
        chain.has_osc52(),
        "OSC 52 is still in the chain as a fallback"
    );
}

/// LOCAL session with NO platform command: OSC 52 is the only real sink, so it
/// leads (and is preferred) even without SSH; temp-file trails.
#[test]
fn default_chain_local_without_platform_keeps_osc52_first() {
    let sink = RecordingSink::new();
    let chain = ClipboardChain::default_chain(
        sink,
        ClipboardCapabilities {
            osc52: true,
            ssh: false,
        },
        vec![],
    );
    assert_eq!(
        chain.providers,
        vec![ClipboardProvider::Osc52, ClipboardProvider::TempFile],
    );
    assert!(
        chain.prefers_osc52(),
        "with no platform command, OSC 52 is preferred even locally"
    );
}

// Touch PLATFORM_OK so the constant isn't flagged unused on some toolchains.
#[test]
fn platform_ok_script_is_well_formed() {
    let s = PLATFORM_OK.clone();
    assert!(s.terminal_error.is_none());
}

// ---------------------------------------------------------------------------
// RealSink: large-payload concurrency (deadlock regression)
// ---------------------------------------------------------------------------

/// A child that floods its stderr pipe *before* it drains stdin would deadlock
/// a sink that writes the whole payload on the calling thread and only then
/// reaps the child: the child blocks writing stderr (its pipe is full because
/// nobody is reading it yet) and never reads the rest of stdin, while the sink
/// blocks writing stdin and never reads stderr. `RealSink::run_command` writes
/// stdin on a separate thread so the two pipes drain concurrently, so this
/// returns instead of hanging. The payload is far larger than any pipe buffer
/// (typically 64 KiB) so the deadlock is forced if the fix regresses.
#[cfg(unix)]
#[test]
fn real_sink_run_command_does_not_deadlock_on_large_payload() {
    use std::sync::mpsc;
    use std::time::Duration;

    // ~512 KiB of payload, comfortably past the pipe buffer.
    let payload = vec![b'x'; 512 * 1024];
    // The child emits ~512 KiB to stderr first (yes >> pipe buffer), then
    // drains stdin via `cat >/dev/null`, then exits non-zero so we also
    // exercise the stderr-surfacing path with a full stderr pipe.
    let script = "yes E | head -c 524288 1>&2; cat >/dev/null; exit 3";

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = RealSink;
        let outcome = sink.run_command("sh", &["-c", script], &payload);
        let _ = tx.send(outcome);
    });

    let outcome = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("run_command must not deadlock on a large payload")
        .expect("sh should spawn and be reaped");
    assert!(!outcome.success, "child exited non-zero");
    assert_eq!(outcome.status_code, Some(3));
    // Stderr was captured despite filling its pipe (proves it drained
    // concurrently with the stdin write).
    assert!(
        outcome.stderr.contains('E'),
        "captured stderr should carry the child's flood, got {:?}",
        outcome.stderr
    );
}

// ---------------------------------------------------------------------------
// RealSink: temp-file is created privately and atomically (security)
// ---------------------------------------------------------------------------

/// `write_temp_file` must not follow/clobber a pre-planted path at its
/// predictable leaf name: `create_new(true)` (O_CREAT|O_EXCL) means a file or
/// symlink already sitting at the target name causes the write to surface an
/// error rather than overwriting it. We pre-create the exact predictable leaf
/// `squeezy-copy-<pid>-<counter>-<label>` and assert the next attempt either
/// errors or routes around the collision — never truncating the pre-planted
/// content.
#[cfg(unix)]
#[test]
fn real_sink_write_temp_file_refuses_to_clobber_preplanted_path() {
    use std::io::Write as _;

    // The shared TEMP_FILE_COUNTER may be advanced by other tests in this same
    // binary between our load and the call, so we cannot pin a single counter.
    // Instead we pre-plant sentinels across the next several predictable leaf
    // names; whichever one the first attempt lands on must be left intact —
    // `create_new` EEXISTs and routes past it, where `File::create` would
    // truncate it to "NEW PAYLOAD".
    let label = "clobber-victim.txt";
    let base = TEMP_FILE_COUNTER.load(Ordering::Relaxed);
    let mut planted: Vec<std::path::PathBuf> = Vec::new();
    for off in 0..8u64 {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "squeezy-copy-{}-{}-{}",
            std::process::id(),
            base + off,
            label,
        ));
        let mut f = std::fs::File::create(&p).expect("plant sentinel");
        f.write_all(b"SENTINEL").expect("write sentinel");
        planted.push(p);
    }

    let mut sink = RealSink;
    let result = sink.write_temp_file(b"NEW PAYLOAD", label);

    // The returned path must NOT be one of the pre-planted sentinels.
    if let Ok(written) = &result {
        assert!(
            !planted.contains(written),
            "must not reuse a pre-planted path; create_new should EEXIST and retry"
        );
    }
    // Every sentinel must still hold its original bytes (none truncated).
    let mut intact = true;
    for p in &planted {
        if std::fs::read(p).ok().as_deref() != Some(b"SENTINEL".as_slice()) {
            intact = false;
        }
        let _ = std::fs::remove_file(p);
    }
    if let Ok(written) = &result {
        let _ = std::fs::remove_file(written);
    }
    assert!(
        intact,
        "pre-planted files must not be truncated/overwritten"
    );
}

/// On Unix the created temp file must be private (0o600), not the umask-derived
/// 0o644 that `File::create` yields, so a clipboard payload is not readable by
/// other local users (CWE-377).
#[cfg(unix)]
#[test]
fn real_sink_write_temp_file_is_mode_0600() {
    use std::os::unix::fs::PermissionsExt;

    let mut sink = RealSink;
    let path = sink
        .write_temp_file(b"secret payload", "mode-check.txt")
        .expect("temp file created");
    let mode = std::fs::metadata(&path)
        .expect("stat temp file")
        .permissions()
        .mode()
        & 0o777;
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        mode, 0o600,
        "clipboard temp file must be 0o600, got {mode:o}"
    );
}

/// A clipboard read that completes within the bound returns its value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_read_returns_value_when_fast() {
    let got = bounded_read(std::time::Duration::from_secs(5), || {
        Some("clip".to_string())
    })
    .await;
    assert_eq!(got, Ok(Some("clip".to_string())));

    let none = bounded_read(std::time::Duration::from_secs(5), || None::<String>).await;
    assert_eq!(none, Ok(None));
}

/// A read that outlives the bound yields `Err(())` (the timed-out path) instead
/// of blocking, so a wedged helper never stalls the event loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_read_times_out_on_slow_helper() {
    let got = bounded_read(std::time::Duration::from_millis(20), || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        Some("never".to_string())
    })
    .await;
    assert_eq!(got, Err(()));
}
