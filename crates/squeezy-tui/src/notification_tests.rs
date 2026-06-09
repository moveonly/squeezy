use std::ffi::OsString;

use super::*;

// ---- DesktopNotifier byte-sequence acceptance ----------------------------

#[test]
fn desktop_notifier_off_emits_nothing() {
    let notifier = DesktopNotifier::new(NotificationMethod::Off);
    let mut buf: Vec<u8> = Vec::new();
    let wrote = notifier
        .write_to(&mut buf, "ignored")
        .expect("write_to should not fail on a Vec");
    assert!(!wrote, "Off must not emit any bytes");
    assert!(buf.is_empty(), "buffer stays empty when notifier is Off");
    assert_eq!(notifier.resolved(), None);
}

#[test]
fn desktop_notifier_bel_emits_bell_byte() {
    let notifier = DesktopNotifier::new(NotificationMethod::Bel);
    let mut buf: Vec<u8> = Vec::new();
    let wrote = notifier.write_to(&mut buf, "turn complete").unwrap();
    assert!(wrote);
    assert_eq!(buf, b"\x07", "Bel writes a single BEL byte");
    assert_eq!(notifier.resolved(), Some(NotificationMethod::Bel));
}

#[test]
fn desktop_notifier_osc9_emits_full_escape_sequence() {
    let notifier = DesktopNotifier::new(NotificationMethod::Osc9);
    let mut buf: Vec<u8> = Vec::new();
    let wrote = notifier.write_to(&mut buf, "turn complete").unwrap();
    assert!(wrote);
    // ESC ] 9 ; <message> BEL — the iTerm-style desktop-notification
    // escape, honoured by Ghostty / Kitty / WezTerm / Warp.
    assert_eq!(buf, b"\x1b]9;turn complete\x07");
    assert_eq!(notifier.resolved(), Some(NotificationMethod::Osc9));
}

#[test]
fn desktop_notifier_osc9_strips_embedded_terminators() {
    // A hostile or accidental BEL / ESC inside the message would either
    // truncate the OSC sequence early (BEL) or break the parser into a
    // new escape (ESC). Both are sanitised out before write.
    let notifier = DesktopNotifier::new(NotificationMethod::Osc9);
    let mut buf: Vec<u8> = Vec::new();
    notifier
        .write_to(&mut buf, "danger\x07inside\x1b[31mred")
        .unwrap();
    assert_eq!(buf, b"\x1b]9;dangerinside[31mred\x07");
}

#[test]
fn desktop_notifier_osc9_collapses_newlines_to_spaces() {
    let notifier = DesktopNotifier::new(NotificationMethod::Osc9);
    let mut buf: Vec<u8> = Vec::new();
    notifier.write_to(&mut buf, "line one\nline two").unwrap();
    assert_eq!(buf, b"\x1b]9;line one line two\x07");
}

#[test]
fn desktop_notifier_auto_resolves_to_a_concrete_backend() {
    let notifier = DesktopNotifier::new(NotificationMethod::Auto);
    let resolved = notifier
        .resolved()
        .expect("Auto must resolve to a concrete backend");
    assert!(matches!(
        resolved,
        NotificationMethod::Bel | NotificationMethod::Osc9
    ));
}

#[test]
fn notification_method_parses_canonical_strings() {
    assert_eq!(
        NotificationMethod::parse("off"),
        Some(NotificationMethod::Off)
    );
    assert_eq!(
        NotificationMethod::parse("bel"),
        Some(NotificationMethod::Bel)
    );
    assert_eq!(
        NotificationMethod::parse("osc9"),
        Some(NotificationMethod::Osc9)
    );
    assert_eq!(
        NotificationMethod::parse("auto"),
        Some(NotificationMethod::Auto)
    );
    assert_eq!(
        NotificationMethod::parse("OSC-9"),
        Some(NotificationMethod::Osc9)
    );
    assert_eq!(
        NotificationMethod::parse("bell"),
        Some(NotificationMethod::Bel)
    );
    assert_eq!(NotificationMethod::parse("nonsense"), None);
}

#[test]
fn osc9_auto_resolves_to_concrete_backend_in_test_env() {
    // In the default test environment $TERM_PROGRAM and Linux signals are
    // absent so Auto should fall back to Bel (not Osc9).
    let notifier = DesktopNotifier::new(NotificationMethod::Auto);
    let resolved = notifier
        .resolved()
        .expect("Auto must always resolve to a concrete backend");
    assert!(
        matches!(resolved, NotificationMethod::Bel | NotificationMethod::Osc9),
        "Auto must resolve to Bel or Osc9, got {resolved:?}"
    );
}

#[test]
fn osc9_detects_term_program_signals() {
    for program in &["iTerm.app", "Ghostty", "WezTerm", "kitty", "WarpTerminal"] {
        let fixture: &[(&str, &str)] = &[("TERM_PROGRAM", program)];
        let lookup = |key: &str| {
            fixture
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(*v))
        };
        assert!(
            detect_osc9_support_from_env(lookup),
            "TERM_PROGRAM={program} should enable OSC9"
        );
    }
}

#[test]
fn osc9_detects_linux_env_var_signals() {
    let signals: &[(&str, &str)] = &[
        ("KITTY_WINDOW_ID", "42"),
        ("WEZTERM_PANE", "0"),
        ("WEZTERM_EXECUTABLE", "/usr/local/bin/wezterm"),
        ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty"),
    ];
    for fixture in signals {
        let signal = fixture.0;
        let fixture_slice: &[(&str, &str)] = std::slice::from_ref(fixture);
        let lookup = |key: &str| {
            fixture_slice
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(*v))
        };
        assert!(
            detect_osc9_support_from_env(lookup),
            "{signal} should enable OSC9"
        );
    }
}

#[test]
fn osc9_detects_linux_term_values() {
    for term in &["xterm-kitty", "ghostty", "wezterm", "foot"] {
        let fixture: &[(&str, &str)] = &[("TERM", term)];
        let lookup = |key: &str| {
            fixture
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(*v))
        };
        assert!(
            detect_osc9_support_from_env(lookup),
            "TERM={term} should enable OSC9"
        );
    }
}

#[test]
fn osc9_off_for_unknown_terminals() {
    let empty = |_: &str| -> Option<OsString> { None };
    assert!(!detect_osc9_support_from_env(empty));

    let dumb: &[(&str, &str)] = &[("TERM", "dumb")];
    assert!(!detect_osc9_support_from_env(|key: &str| {
        dumb.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| OsString::from(*v))
    }));

    let screen: &[(&str, &str)] = &[("TERM", "screen-256color")];
    assert!(!detect_osc9_support_from_env(|key: &str| {
        screen
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| OsString::from(*v))
    }));
}

#[test]
fn osc9_off_when_tmux_is_outer_term_program_without_capability_signals() {
    // Under tmux 3.3+ `$TERM_PROGRAM` is sometimes overwritten to "tmux".
    // If the underlying emulator did not export any of the Linux capability
    // signals (KITTY_WINDOW_ID / WEZTERM_* / GHOSTTY_RESOURCES_DIR) and
    // $TERM is a plain screen/tmux value, OSC9 must NOT be auto-enabled —
    // we can't prove the outer emulator honours it.
    let fixture: &[(&str, &str)] = &[
        ("TERM_PROGRAM", "tmux"),
        ("TERM", "tmux-256color"),
        ("TMUX", "/tmp/tmux-1000/default,1234,0"),
    ];
    let lookup = |key: &str| {
        fixture
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| OsString::from(*v))
    };
    assert!(
        !detect_osc9_support_from_env(lookup),
        "TERM_PROGRAM=tmux without a capability signal must keep OSC9 off",
    );
}
