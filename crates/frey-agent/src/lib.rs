//! The Frey agent loop.
//!
//! A model, some tools, a context plan, a cost ledger, and a journal — the smallest thing that is
//! actually an agent rather than a wrapper around a chat endpoint.
//!
//! Two properties are worth knowing before reading the code:
//!
//! * **The journal is the session.** Resuming replays it; there is no second copy of the state that
//!   can drift from the transcript.
//! * **Nothing degrades quietly.** Eviction, cache churn, truncation, a missing capability, and a
//!   fatal provider failure all produce something the caller can see, because the alternative is a
//!   run that appears to work and does not.

pub mod journal;
pub mod multi;
pub mod run;

/// The types most callers want.
pub mod prelude {
    pub use crate::journal::{Journal, Replay, ReplayError};
    pub use crate::multi::{Child, Inheritance, Spawn, SpawnError, spawn};
    pub use crate::run::{Agent, RunError, RunOutput, ToolHost};
}
