//! Unit tests for the pure Replayable Interaction Macros model (§12.3.7).
//!
//! These exercise the recorder/replayer state machine in isolation — record
//! start/stop, the committed-command capture, the noise-is-ignored contract (the
//! recorder simply does nothing when fed while idle/replaying), replay pumping,
//! cancellation, the cap, and the visible status — without a terminal.

use super::*;
use crate::keymap::Action;

#[test]
fn fresh_recorder_is_idle_and_inert() {
    let rec = MacroRecorder::new();
    assert!(!rec.is_active(), "fresh recorder is idle");
    assert!(!rec.is_recording());
    assert!(!rec.is_replaying());
    assert!(!rec.has_replayable(), "nothing recorded yet");
    assert_eq!(rec.recording_len(), 0);
    assert_eq!(rec.replay_progress(), (0, 0));
    assert!(rec.status_line().is_none(), "idle paints no strip");
}

#[test]
fn note_command_while_idle_is_ignored() {
    // The caller forwards every committed action unconditionally; the recorder
    // must ignore the stream entirely while not recording. This is the
    // "noise events are ignored" contract at the model boundary.
    let mut rec = MacroRecorder::new();
    rec.note_command(Action::ToggleMinimap);
    rec.note_command(Action::CopyViewport);
    assert!(!rec.is_active());
    assert!(!rec.has_replayable());
}

#[test]
fn record_capture_stop_stores_macro() {
    let mut rec = MacroRecorder::new();
    assert!(rec.start_recording(), "arming a fresh recording succeeds");
    assert!(rec.is_recording());
    assert_eq!(rec.recording_len(), 0);

    rec.note_command(Action::ToggleMinimap);
    rec.note_command(Action::ToggleSoftWrap);
    rec.note_command(Action::CopyViewport);
    assert_eq!(rec.recording_len(), 3, "three committed commands captured");

    let outcome = rec.stop_recording();
    assert_eq!(
        outcome,
        StopOutcome::Stored {
            name: "macro 1".to_string(),
            len: 3,
            truncated: false,
        }
    );
    assert!(!rec.is_active(), "stopped → idle");
    assert!(rec.has_replayable(), "the macro is stored for replay");
}

#[test]
fn double_start_is_a_noop_and_keeps_recording() {
    let mut rec = MacroRecorder::new();
    assert!(rec.start_recording());
    rec.note_command(Action::ToggleMinimap);
    // Starting again while recording must NOT wipe the in-progress capture.
    assert!(!rec.start_recording(), "re-arm is refused");
    assert_eq!(rec.recording_len(), 1, "the captured command survives");
}

#[test]
fn empty_recording_is_discarded_not_stored() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    assert_eq!(rec.stop_recording(), StopOutcome::Empty);
    assert!(!rec.has_replayable(), "an empty record/stop stores nothing");
}

#[test]
fn stop_without_recording_is_a_noop() {
    let mut rec = MacroRecorder::new();
    assert_eq!(rec.stop_recording(), StopOutcome::NotRecording);
}

#[test]
fn replay_pumps_every_command_in_order_then_returns_idle() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    rec.note_command(Action::CopyViewport);
    rec.stop_recording();

    assert!(rec.begin_replay(), "a stored macro replays");
    assert!(rec.is_replaying());
    assert_eq!(rec.replay_progress(), (0, 2), "0 of 2 done");

    assert_eq!(rec.next_replay_command(), Some(Action::ToggleMinimap));
    assert_eq!(rec.next_replay_command(), Some(Action::CopyViewport));
    assert_eq!(
        rec.next_replay_command(),
        None,
        "exhausted macro yields None"
    );
    assert!(!rec.is_active(), "replay completion returns to idle");
}

#[test]
fn note_command_while_replaying_is_ignored() {
    // A replayed step re-dispatches through the keymap; if that step were itself
    // recordable it must NOT mutate the in-flight replay (the recorder is not
    // recording during replay).
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    rec.stop_recording();
    rec.begin_replay();
    rec.note_command(Action::CopyViewport); // ignored — not recording
    assert_eq!(
        rec.next_replay_command(),
        Some(Action::ToggleMinimap),
        "the replay still yields exactly the recorded command"
    );
    assert_eq!(rec.next_replay_command(), None);
}

#[test]
fn begin_replay_with_nothing_recorded_fails() {
    let mut rec = MacroRecorder::new();
    assert!(!rec.begin_replay(), "nothing to replay");
    assert!(!rec.is_active());
}

#[test]
fn cancel_aborts_an_in_progress_recording_without_storing() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    assert!(rec.cancel(), "cancel reports it did something");
    assert!(!rec.is_active());
    assert!(
        !rec.has_replayable(),
        "a cancelled recording is discarded, not stored"
    );
}

#[test]
fn cancel_stops_a_replay_mid_run() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    rec.note_command(Action::CopyViewport);
    rec.stop_recording();
    rec.begin_replay();
    let _ = rec.next_replay_command(); // consume one
    assert!(rec.cancel(), "cancel stops the replay");
    assert!(!rec.is_active());
    assert_eq!(
        rec.next_replay_command(),
        None,
        "a cancelled replay yields nothing further"
    );
}

#[test]
fn cancel_while_idle_is_a_noop() {
    let mut rec = MacroRecorder::new();
    assert!(!rec.cancel(), "nothing to cancel when idle");
}

#[test]
fn recording_is_capped_and_flags_truncation() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    // Push past the cap; extras are dropped and the truncation latches.
    for _ in 0..(MAX_MACRO_LEN + 5) {
        rec.note_command(Action::ToggleMinimap);
    }
    assert_eq!(
        rec.recording_len(),
        MAX_MACRO_LEN,
        "capture stops at the cap"
    );
    let outcome = rec.stop_recording();
    match outcome {
        StopOutcome::Stored { len, truncated, .. } => {
            assert_eq!(len, MAX_MACRO_LEN);
            assert!(truncated, "the clip is reported so the status can say so");
        }
        other => panic!("expected Stored, got {other:?}"),
    }
}

#[test]
fn successive_recordings_get_distinct_names() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    let first = rec.stop_recording();
    rec.start_recording();
    rec.note_command(Action::CopyViewport);
    let second = rec.stop_recording();
    let name_of = |o: &StopOutcome| match o {
        StopOutcome::Stored { name, .. } => name.clone(),
        _ => panic!("expected Stored"),
    };
    assert_eq!(name_of(&first), "macro 1");
    assert_eq!(name_of(&second), "macro 2", "names are distinct");
}

#[test]
fn second_recording_replaces_the_replayable_macro() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    rec.stop_recording();
    rec.start_recording();
    rec.note_command(Action::CopyViewport);
    rec.note_command(Action::CopyFullTranscript);
    rec.stop_recording();
    // Replay yields the SECOND macro's commands.
    rec.begin_replay();
    assert_eq!(rec.next_replay_command(), Some(Action::CopyViewport));
    assert_eq!(rec.next_replay_command(), Some(Action::CopyFullTranscript));
    assert_eq!(rec.next_replay_command(), None);
}

#[test]
fn status_line_reports_record_and_replay_progress() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    rec.note_command(Action::CopyViewport);
    let recording = rec.status_line().expect("recording paints a strip");
    assert!(recording.contains("REC"), "record strip: {recording}");
    assert!(recording.contains('2'), "step count shown: {recording}");

    rec.stop_recording();
    rec.begin_replay();
    let _ = rec.next_replay_command();
    let replaying = rec.status_line().expect("replay paints a strip");
    assert!(replaying.contains("replay"), "replay strip: {replaying}");
    assert!(
        replaying.contains("1/2"),
        "replay shows done/total: {replaying}"
    );
}

#[test]
fn cannot_start_recording_while_replaying() {
    let mut rec = MacroRecorder::new();
    rec.start_recording();
    rec.note_command(Action::ToggleMinimap);
    rec.stop_recording();
    rec.begin_replay();
    assert!(
        !rec.start_recording(),
        "recording is refused while a replay is in flight"
    );
    assert!(rec.is_replaying(), "the replay is left untouched");
}
