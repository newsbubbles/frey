//! Agent2Agent v1.0 for [Frey](https://github.com/newsbubbles/frey).
//!
//! A2A is worth supporting for one technical reason beyond interoperability: its task lifecycle has
//! **`INPUT_REQUIRED` and `AUTH_REQUIRED` as interrupted, non-terminal states**, which is the same
//! shape as MCP's multi round-trip pattern and AG-UI's interrupt. Three independent committees
//! arrived at one concept, so Frey models it once ([`frey_core::error::NeedsInput`]) and projects it
//! three ways. Building A2A after the fact would have meant discovering that too late.
//!
//! A peer is not a trusted party. Its replies are model output wearing a task envelope, and TLS
//! proves who said something rather than whether it is true, so everything from a peer arrives
//! labelled low-integrity — signed agent card or not.

pub mod card;
pub mod task;

/// The types most callers want.
pub mod prelude {
    pub use crate::card::{AgentCard, AgentSkill, Capabilities, WELL_KNOWN_PATH};
    pub use crate::task::{Part, Task, TaskState, TaskStatus};
}
