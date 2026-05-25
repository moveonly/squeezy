//! Agent-driven QA harness for Squeezy.
//!
//! `squeezy-eval` lets an external agent run a scripted scenario against the
//! real `squeezy-agent` loop, capture every event/perf/text frame the run
//! produces, and (optionally) ask an LLM to turn the trace into draft tickets.
//!
//! It is a peer to `squeezy-harness`, not a replacement: harness stays
//! mock-trace deterministic for CI; eval is live-agent for exploratory QA.

pub mod capture;
pub mod driver;
pub mod frames;
pub mod scenario;
pub mod tickets;
pub mod triage;
pub mod workspace;

pub use capture::{Capture, EvalEvent, EvalEventKind};
pub use driver::{RunOptions, RunOutcome, run_scenario};
pub use frames::FrameRecord;
pub use scenario::{Action, Expect, Scenario, Step, TriageConfig, WorkspaceSpec};
pub use tickets::TicketDraft;
pub use workspace::{ProvisionedWorkspace, WorkspaceSource};
