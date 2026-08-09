//! Harness plumbing for [Frey](https://github.com/newsbubbles/frey).
//!
//! In 2026 the shape people ship is a *harness*, not a chatbot: a loop, tools bound to a workspace,
//! approval gates, a streaming UI, sessions that resume, and visible cost. None of that is
//! model-specific, and all of it is what gets rebuilt badly each time. This crate is that skeleton.
//!
//! Three things here are worth knowing before reading further:
//!
//! * **The journal is the session.** Resuming replays it; forking branches it.
//! * **AG-UI needs a serialiser, not an adapter**, because the internal event bus was built in its
//!   shape (ADR-0015).
//! * **`doctor`'s JSON output is an API.** A coding agent parses it to orient in an unfamiliar
//!   project, so it is snapshot-tested like any other contract.

pub mod agui;
pub mod doctor;
pub mod session;

/// The types most callers want.
pub mod prelude {
    pub use crate::agui::{Frame, project, project_stream};
    pub use crate::doctor::{Finding, Report, Severity};
    pub use crate::session::{ApprovalPolicy, HarnessError, Session, Surface, validate};
}
