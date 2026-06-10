//! Replayable Interaction Macros (§12.3.7): record a short sequence of
//! *committed* interactions as a named macro and replay it on demand.
//!
//! **Logical commands, never coordinates.** A macro records the canonical
//! [`keymap::Action`] each interaction committed to — the same logical command
//! the keyboard and the mouse both dispatch through `dispatch_keymap_action` —
//! NOT a terminal row/column or a raw keystroke. Replay re-feeds those recorded
//! actions through the *exact same dispatcher*, so a replayed step is
//! indistinguishable from the user pressing the chord (or clicking the
//! affordance) themselves. This is what the spec means by "record logical
//! commands, not terminal coordinates": a macro recorded on an 80x24 terminal
//! replays correctly on a 200x50 one, and an approval gate a step would hit is
//! hit on replay exactly as it would be live — replay never bypasses approvals.
//!
//! **Pure model.** Like the other §12 leaf modules (`first_run_hints`,
//! `breadcrumbs`, `hover_intent`), this file owns only a tiny state machine and
//! does NOT depend on `lib.rs`'s `TuiApp`. The caller (`lib.rs`) feeds it
//! committed commands ([`MacroRecorder::note_command`]), starts/stops recording
//! explicitly ([`start_recording`]/[`stop_recording`]), and pumps replay one
//! step at a time ([`begin_replay`]/[`next_replay_command`]). Because the caller
//! only ever forwards a *committed* `keymap::Action`, the noise the spec calls
//! out — hover, mouse-move, ticks, resize, toasts — never reaches the recorder:
//! those events resolve to no `keymap::Action` and so are structurally ignored.
//!
//! **Explicit start/stop, visible, cancellable.** Recording is armed and
//! disarmed by a dedicated keymap verb (never implicitly), so a session that
//! never records pays nothing. Replay walks the recorded list one command per
//! pump with a visible progress indicator and is cancellable mid-run (Esc or the
//! same toggle verb) — the spec's "automation must be visible and cancellable".
//!
//! **Zero idle cost.** The resting state is [`MacroState::Idle`]; [`is_active`]
//! short-circuits on a single enum-tag check, so an idle session that is neither
//! recording nor replaying does no work and schedules no redraw.
//!
//! [`start_recording`]: MacroRecorder::start_recording
//! [`stop_recording`]: MacroRecorder::stop_recording
//! [`begin_replay`]: MacroRecorder::begin_replay
//! [`next_replay_command`]: MacroRecorder::next_replay_command
//! [`is_active`]: MacroRecorder::is_active

use crate::keymap::Action;

/// The maximum number of commands a single macro retains. A macro is a *short*
/// workflow (the spec lists "search, select, copy, queue, export, fold, and
/// navigation"), so a generous-but-bounded cap keeps a runaway recording (a user
/// who forgets recording is armed) from growing without limit. Commands past the
/// cap are dropped with the recording flagged truncated so the status can say so.
pub(crate) const MAX_MACRO_LEN: usize = 64;

/// One recorded logical command: the canonical [`keymap::Action`] an interaction
/// committed to. A newtype (not a bare `Action`) so the macro layer carries its
/// own vocabulary and a future richer command (e.g. a parameterised target) can
/// be added without touching every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordedCommand(pub(crate) Action);

impl RecordedCommand {
    /// The keymap action this command replays as — fed straight back into
    /// `dispatch_keymap_action` so a replayed step takes the identical path the
    /// live keyboard/mouse press took.
    pub(crate) fn action(self) -> Action {
        self.0
    }
}

/// A finished, replayable macro: a stable name plus the ordered logical commands
/// it captured. Held by the recorder as the "last recorded" macro so the replay
/// verb has something to run; cheap to clone (a short `Vec` of `Copy` actions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InteractionMacro {
    /// A short human-facing name surfaced in the record/replay status strip.
    pub(crate) name: String,
    /// The ordered commands, in the order they were committed.
    pub(crate) commands: Vec<RecordedCommand>,
}

impl InteractionMacro {
    /// True when the macro recorded no commands (a record/stop with no
    /// interaction between). Such a macro is discarded rather than stored.
    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// The recorder's state machine. The resting variant is [`MacroState::Idle`];
/// the other two are entered only by the explicit record / replay verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MacroState {
    /// Neither recording nor replaying — the zero-cost resting state.
    Idle,
    /// Recording is armed; `commands` accumulates each committed command (until
    /// the cap), `truncated` latches once the cap was hit so the status can say
    /// the recording was clipped.
    Recording {
        commands: Vec<RecordedCommand>,
        truncated: bool,
    },
    /// A macro is replaying; `commands` is the snapshot being walked and `cursor`
    /// is the index of the NEXT command to emit. Replay completes (returns to
    /// Idle) when `cursor == commands.len()`.
    Replaying {
        commands: Vec<RecordedCommand>,
        cursor: usize,
    },
}

/// The macro recorder/replayer. One per session, held by `TuiApp`. Records the
/// committed [`keymap::Action`] stream while armed, stores the most recent
/// finished macro, and pumps it back through the dispatcher on replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MacroRecorder {
    state: MacroState,
    /// The most recently recorded non-empty macro, available to replay. `None`
    /// until the first recording is stopped with at least one command.
    last: Option<InteractionMacro>,
    /// Monotonic counter feeding the default macro name, so successive recordings
    /// get distinct names ("macro 1", "macro 2", …) without the model needing a
    /// clock (which would risk the Windows back-dated-`Instant` panic the render
    /// modules guard against).
    next_ordinal: u32,
}

impl Default for MacroRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// The outcome of stopping a recording, so the caller can set an honest status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// Recording was not armed; nothing happened.
    NotRecording,
    /// Recording stopped but captured no commands; nothing was stored.
    Empty,
    /// Recording stopped and stored a macro of `len` commands under `name`;
    /// `truncated` is true when the cap clipped it.
    Stored {
        name: String,
        len: usize,
        truncated: bool,
    },
}

impl MacroRecorder {
    pub(crate) fn new() -> Self {
        Self {
            state: MacroState::Idle,
            last: None,
            next_ordinal: 1,
        }
    }

    /// True when the recorder is doing anything (recording or replaying). The
    /// single-tag check the idle redraw gate consults — when this is false the
    /// caller does no macro work for the frame.
    pub(crate) fn is_active(&self) -> bool {
        !matches!(self.state, MacroState::Idle)
    }

    /// True while recording is armed.
    pub(crate) fn is_recording(&self) -> bool {
        matches!(self.state, MacroState::Recording { .. })
    }

    /// True while a macro is replaying.
    pub(crate) fn is_replaying(&self) -> bool {
        matches!(self.state, MacroState::Replaying { .. })
    }

    /// Whether a previously recorded macro is available to replay.
    pub(crate) fn has_replayable(&self) -> bool {
        self.last.is_some()
    }

    /// The number of commands recorded so far in the *current* recording (zero
    /// when not recording). A test/diagnostic aid; the live count the user sees is
    /// the [`status_line`] "● REC macro — N step(s)".
    ///
    /// [`status_line`]: MacroRecorder::status_line
    #[cfg(test)]
    pub(crate) fn recording_len(&self) -> usize {
        match &self.state {
            MacroState::Recording { commands, .. } => commands.len(),
            _ => 0,
        }
    }

    /// Replay progress as `(done, total)` — the count already emitted and the
    /// macro's length. `(0, 0)` when not replaying. A test/diagnostic aid; the
    /// visible progress the user sees is the [`status_line`] "▶ replay macro —
    /// done/total".
    ///
    /// [`status_line`]: MacroRecorder::status_line
    #[cfg(test)]
    pub(crate) fn replay_progress(&self) -> (usize, usize) {
        match &self.state {
            MacroState::Replaying { commands, cursor } => (*cursor, commands.len()),
            _ => (0, 0),
        }
    }

    /// Arm recording. Idempotent-safe: starting while already recording keeps the
    /// in-progress recording untouched and returns `false` so the caller can say
    /// "already recording". Starting while replaying is refused (also `false`) —
    /// the caller cancels the replay first. Returns `true` when a fresh recording
    /// was armed.
    pub(crate) fn start_recording(&mut self) -> bool {
        if !matches!(self.state, MacroState::Idle) {
            return false;
        }
        self.state = MacroState::Recording {
            commands: Vec::new(),
            truncated: false,
        };
        true
    }

    /// Feed one *committed* logical command into the recorder. A no-op unless
    /// recording is armed, so the caller can forward every dispatched
    /// `keymap::Action` unconditionally and the recorder ignores the stream when
    /// idle or replaying. Drops the command (latching `truncated`) once the macro
    /// hits [`MAX_MACRO_LEN`], so a forgotten recording can never grow unbounded.
    ///
    /// Noise events (hover, mouse-move, ticks, resize, toasts) are ignored by
    /// construction: they resolve to no `keymap::Action`, so the caller never
    /// reaches this method for them.
    pub(crate) fn note_command(&mut self, action: Action) {
        if let MacroState::Recording {
            commands,
            truncated,
        } = &mut self.state
        {
            if commands.len() >= MAX_MACRO_LEN {
                *truncated = true;
                return;
            }
            commands.push(RecordedCommand(action));
        }
    }

    /// Disarm recording. Stores the captured commands as the new "last" macro
    /// when at least one was recorded (so it can be replayed), or discards an
    /// empty recording. Returns a [`StopOutcome`] so the caller sets an honest
    /// status. A no-op (`NotRecording`) when recording was not armed.
    pub(crate) fn stop_recording(&mut self) -> StopOutcome {
        // Only move out of `Recording`; leave a replay (or Idle) untouched so
        // `stop_recording` can never wrongly cancel an in-progress replay.
        if !self.is_recording() {
            return StopOutcome::NotRecording;
        }
        let MacroState::Recording {
            commands,
            truncated,
        } = std::mem::replace(&mut self.state, MacroState::Idle)
        else {
            // Unreachable: `is_recording()` above guaranteed the Recording arm.
            return StopOutcome::NotRecording;
        };
        if commands.is_empty() {
            return StopOutcome::Empty;
        }
        let name = format!("macro {}", self.next_ordinal);
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        let len = commands.len();
        self.last = Some(InteractionMacro {
            name: name.clone(),
            commands,
        });
        StopOutcome::Stored {
            name,
            len,
            truncated,
        }
    }

    /// Begin replaying the most recently recorded macro. Returns `true` when a
    /// non-empty macro was loaded for replay; `false` when there is nothing to
    /// replay or the recorder is busy (recording or already replaying). The
    /// caller then pumps [`next_replay_command`] until it returns `None`.
    ///
    /// [`next_replay_command`]: MacroRecorder::next_replay_command
    pub(crate) fn begin_replay(&mut self) -> bool {
        if !matches!(self.state, MacroState::Idle) {
            return false;
        }
        let Some(macro_def) = self.last.as_ref() else {
            return false;
        };
        if macro_def.is_empty() {
            return false;
        }
        self.state = MacroState::Replaying {
            commands: macro_def.commands.clone(),
            cursor: 0,
        };
        true
    }

    /// Advance the replay by one command, returning the [`keymap::Action`] to
    /// dispatch, or `None` when the macro is exhausted (which returns the
    /// recorder to [`MacroState::Idle`]). The caller dispatches the returned
    /// action through `dispatch_keymap_action` — the same path a live press
    /// takes — then pumps again. A no-op (`None`) when not replaying.
    pub(crate) fn next_replay_command(&mut self) -> Option<Action> {
        let MacroState::Replaying { commands, cursor } = &mut self.state else {
            return None;
        };
        if *cursor >= commands.len() {
            self.state = MacroState::Idle;
            return None;
        }
        let action = commands[*cursor].action();
        *cursor += 1;
        if *cursor >= commands.len() {
            // Last command emitted — return to Idle so a follow-up pump is a
            // clean no-op and `is_active` reads false immediately.
            self.state = MacroState::Idle;
        }
        Some(action)
    }

    /// Cancel whatever the recorder is doing — abort an in-progress recording
    /// (discarding it, NOT storing) or stop a replay mid-run — returning the
    /// recorder to [`MacroState::Idle`]. Returns `true` when something was
    /// cancelled. The spec's "replay must be cancellable" twin: Esc / the toggle
    /// verb routes here.
    pub(crate) fn cancel(&mut self) -> bool {
        if matches!(self.state, MacroState::Idle) {
            return false;
        }
        self.state = MacroState::Idle;
        true
    }

    /// A short status describing what the recorder is doing this frame, or `None`
    /// when idle. Drives the non-modal status strip the render path paints.
    pub(crate) fn status_line(&self) -> Option<String> {
        match &self.state {
            MacroState::Idle => None,
            MacroState::Recording { commands, .. } => {
                Some(format!("● REC macro — {} step(s)", commands.len()))
            }
            MacroState::Replaying { commands, cursor } => Some(format!(
                "▶ replay macro — {}/{}",
                (*cursor).min(commands.len()),
                commands.len()
            )),
        }
    }
}

#[cfg(test)]
#[path = "macros_tests.rs"]
mod tests;
