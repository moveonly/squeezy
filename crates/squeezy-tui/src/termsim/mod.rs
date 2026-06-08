//! Term-matrix framework (§8 of `TUI_ALT_SCREEN_RENDERER_PLAN.md`).
//!
//! A scenario × terminal-emulator matrix that replays the TUI's real
//! append-only ANSI stream through multiple emulators and asserts the §8.5
//! invariants (single composer horizon, no duplicate dividers, latest response
//! survives resize, cursor stays in bounds). It exists to prove the alt-screen
//! migration against the VS Code / xterm.js cursor-drift regression.
//!
//! The whole tree is gated behind the `term-matrix` feature, so the release
//! library and the default `cargo test -p squeezy-tui` never compile the
//! emulator crates (`vt100`, `alacritty_terminal`, `insta`). `term-matrix`
//! implies `testing` because [`driver`] drives the `TuiHarness`.
//!
//! ## Layout
//!
//! * [`types`] — pure data shapes (`CaptureLog`, `FrameMark`, `Grid`,
//!   `EmulatorProfile`), no emulator crate in scope.
//! * [`emulator`] — the [`emulator::Emulator`] trait + shared replay helpers
//!   (per-frame splitting / width reconstruction).
//! * [`backend_vt100`] / [`backend_alacritty`] — the two always-on Rust legs.
//! * [`scenario`] — the `Step` enum, `Scenario` model, and shipped registry.
//! * [`driver`] — the only file touching `TuiHarness`; produces a `CaptureLog`.
//! * [`assertions`] — the §8.5 invariant checks over a `Grid`.
//! * [`matrix`] — the cartesian runner + the feature-gated `#[test]`.
//!
//! This is the COMPILES-FIRST scaffold: every backend / driver / assertion is
//! a stub returning `Default`/`Ok`. The not-yet-wired surface area means a
//! tree-wide `allow(dead_code)` keeps `-D warnings` builds green until the
//! real replay + driver land.
#![allow(dead_code)]

mod assertions;
mod backend_alacritty;
mod backend_vt100;
mod driver;
mod emulator;
mod export;
mod matrix;
mod scenario;
mod types;

// The real-ANSI capture harness lives in `crate::testing` (gated behind
// `term-matrix`), not in `driver.rs`. Re-export the capture data shapes
// it consumes so the sibling `testing` module can name them without the
// submodule internals (the emulator/scenario registry) leaking out.
#[cfg(feature = "term-matrix")]
pub(crate) use scenario::Step;
#[cfg(feature = "term-matrix")]
pub(crate) use types::{CaptureLog, FrameMark};
