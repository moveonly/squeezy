//! Scenario model: the scripted [`Step`] sequence a [`Scenario`] replays,
//! plus the registry of shipped scenarios (§8.2 / §8.E).
//!
//! Each `Step` mirrors a production event source so the captured byte stream
//! is identical to a real session. The driver (`driver.rs`) interprets these
//! against a `TuiHarness` + `FixedSize` + `Capture` sink.
//!
//! Scaffolding: [`shipped_scenarios`] returns the six named scenarios with
//! empty step lists for now; the steps are filled in once the driver lands.

use crossterm::event::KeyEvent;

/// One scripted action. Mirrors the production event sources.
#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// crossterm `KeyEvent` routed at the real `handle_key`, pumping idle
    /// before and after exactly like `send_key`.
    Key(KeyEvent),
    /// A mouse event (selection / scroll), once mouse routing lands.
    Mouse,
    /// Bracketed-paste payload injected the way the paste handler receives it.
    Paste(String),
    /// Swap the `FixedSize` source to `(w, h)` and set `app.pending_resize`,
    /// mirroring `Event::Resize`. The next painted frame re-reads size and
    /// reflows the footer.
    Resize(u16, u16),
    /// One `pump_until_idle` pass: lets a mid-stream turn advance without a key.
    Tick,
    /// Push assistant text as the model would, driving the streaming surface.
    AssistantDelta(String),
    /// Inject a tool-output transcript entry the way a completed tool call lands.
    ToolOutput(String),
    /// Run `pump_until_idle` to completion so the turn settles and history
    /// flushes (the settle boundary that gates history commit).
    SettleTurn,
    /// Open the fullscreen transcript overlay (Ctrl+T): flips to the
    /// alt-screen terminal and uses `render` rather than `render_inline`.
    OpenOverlay,
    /// Close the overlay; the next paint resumes the append-only main path.
    CloseOverlay,
    /// Force one paint of the current state and record a `FrameMark` at the
    /// current byte offset. The only step that emits a marker.
    Frame,
    // Future steps once those surfaces land:
    // CopyCommand, Search(String),
}

/// A named, ordered list of [`Step`]s the driver replays end to end.
#[derive(Debug, Clone)]
pub(crate) struct Scenario {
    /// Stable identifier used in matrix output and snapshot names.
    pub name: &'static str,
    /// Initial terminal size `(cols, rows)` before the first step.
    pub initial_size: (u16, u16),
    /// The scripted steps, in order.
    pub steps: Vec<Step>,
}

/// A known tail substring of the latest assistant response a scenario
/// commits, threaded into [`crate::termsim::assertions::latest_response_present`]
/// so the post-resize "history not lost" check has a concrete needle. `None`
/// for scenarios that never commit assistant text (e.g. bare `startup`).
impl Scenario {
    /// The tail of the last `AssistantDelta` in `steps`, if any — the needle
    /// the latest-response invariant searches for after a resize. Computed
    /// from the script so it can never drift from what the driver actually
    /// injects.
    pub(crate) fn latest_response_tail(&self) -> Option<String> {
        self.steps.iter().rev().find_map(|step| match step {
            Step::AssistantDelta(text) => {
                // Use a short, single-word tail so reflow/clipping at narrow
                // widths can't split the needle across a wrapped row boundary.
                text.split_whitespace().last().map(str::to_string)
            }
            _ => None,
        })
    }
}

/// The six scenarios to ship first (§8.E), smallest blast radius first.
///
/// Each scenario scripts production event sources (`Step`s) ending in at least
/// one `Step::Frame` so the matrix has a settled frame to assert against. The
/// names and ordering are the contract the matrix runner and snapshot files
/// key on.
pub(crate) fn shipped_scenarios() -> Vec<Scenario> {
    // A multi-line assistant body big enough to exercise the streaming/commit
    // surface and survive a resize. Distinct tail word per scenario so the
    // latest-response needle is unambiguous.
    fn assistant_body(tail: &str) -> String {
        format!(
            "Here is a multi-line answer.\n\
             It spans several lines so the history commit and footer\n\
             reflow both have real content to move around. {tail}"
        )
    }

    vec![
        // 1. Picker dismissed -> first frame. Single composer horizon on boot.
        Scenario {
            name: "startup",
            initial_size: (120, 40),
            steps: vec![Step::SettleTurn, Step::Frame],
        },
        // 2. One multi-line streaming delta that settles. Latest-response +
        //    no-duplicate-divider on the simplest committed turn.
        Scenario {
            name: "single_turn",
            initial_size: (120, 40),
            steps: vec![
                Step::Frame,
                Step::AssistantDelta(assistant_body("singleturndone")),
                Step::SettleTurn,
                Step::Frame,
            ],
        },
        // 3. Resize(140 -> 90 -> 140), H stable. Minimal reflow trigger.
        Scenario {
            name: "shrink_then_grow",
            initial_size: (140, 40),
            steps: vec![
                Step::AssistantDelta(assistant_body("shrinkgrowdone")),
                Step::SettleTurn,
                Step::Frame,
                Step::Resize(90, 40),
                Step::Frame,
                Step::Resize(140, 40),
                Step::Frame,
            ],
        },
        // 4. W oscillates ~250<->195. The real VS Code 22-stack trigger.
        Scenario {
            name: "width_drag_storm",
            initial_size: (250, 40),
            steps: vec![
                Step::AssistantDelta(assistant_body("widthstormdone")),
                Step::SettleTurn,
                Step::Frame,
                Step::Resize(195, 40),
                Step::Frame,
                Step::Resize(250, 40),
                Step::Frame,
                Step::Resize(210, 40),
                Step::Frame,
                Step::Resize(248, 40),
                Step::Frame,
                Step::Resize(196, 40),
                Step::Frame,
            ],
        },
        // 5. H oscillates (e.g. 64<->12). Composer-pin / clamp under
        //    vertical pressure; cursor-in-[0,h) bound.
        Scenario {
            name: "height_storm",
            initial_size: (120, 64),
            steps: vec![
                Step::AssistantDelta(assistant_body("heightstormdone")),
                Step::SettleTurn,
                Step::Frame,
                Step::Resize(120, 12),
                Step::Frame,
                Step::Resize(120, 64),
                Step::Frame,
                Step::Resize(120, 8),
                Step::Frame,
                Step::Resize(120, 48),
                Step::Frame,
            ],
        },
        // 6. OpenOverlay (Ctrl+T) -> scroll -> CloseOverlay (Esc). Overlay and
        //    main view share one surface; clean return with one horizon.
        Scenario {
            name: "overlay_round_trip",
            initial_size: (120, 40),
            steps: vec![
                Step::AssistantDelta(assistant_body("overlaydone")),
                Step::SettleTurn,
                Step::Frame,
                Step::OpenOverlay,
                Step::Frame,
                Step::CloseOverlay,
                Step::Frame,
            ],
        },
    ]
}
