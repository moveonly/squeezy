use super::*;

#[test]
fn from_name_resolves_builtins_and_aliases() {
    assert_eq!(SpinnerStyle::from_name("twinkle"), SpinnerStyle::Twinkle);
    assert_eq!(
        SpinnerStyle::from_name("scintillate"),
        SpinnerStyle::Scintillate
    );
    assert_eq!(SpinnerStyle::from_name("drift"), SpinnerStyle::Drift);
    // aliases + casing
    assert_eq!(
        SpinnerStyle::from_name("Sparkle"),
        SpinnerStyle::Scintillate
    );
    assert_eq!(SpinnerStyle::from_name("shooting"), SpinnerStyle::Drift);
    // unknown falls back to the default style (drift)
    assert_eq!(SpinnerStyle::from_name("nope"), SpinnerStyle::Drift);
    assert_eq!(SpinnerStyle::default(), SpinnerStyle::Drift);
}

#[test]
fn frame_cycles_through_every_phase() {
    let style = SpinnerStyle::Twinkle;
    let frames = style.frames();
    // Advancing by one interval steps exactly one frame, wrapping around.
    for i in 0..frames.len() * 2 {
        let elapsed = i as u64 * style.interval_ms();
        assert_eq!(style.frame(elapsed), frames[i % frames.len()]);
    }
}

#[test]
fn every_style_has_frames() {
    for style in [
        SpinnerStyle::Twinkle,
        SpinnerStyle::Scintillate,
        SpinnerStyle::Drift,
    ] {
        assert!(!style.frames().is_empty());
        assert!(style.interval_ms() > 0);
        // frame(0) must be the first phase of the cycle.
        assert_eq!(style.frame(0), style.frames()[0]);
    }
}
