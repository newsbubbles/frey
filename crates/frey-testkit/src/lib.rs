//! Test doubles and assertions for [Frey](https://github.com/newsbubbles/frey).
//!
//! This crate exists **before** the first provider adapter, on purpose. A provider written without
//! a scripted model to test against gets written against a live API, and then never gets tested
//! properly afterwards. The build plan makes this milestone three and the first provider milestone
//! five for exactly that reason.
//!
//! It is a normal published crate rather than a test module, because anyone building an agent on
//! Frey needs the same doubles to test *their* agent.
//!
//! # What is here
//!
//! * [`scripted::ScriptedModel`] — a `ModelProvider` that returns canned turns and, more usefully,
//!   records what it was shown so a test can assert on tool order and cache breakpoints.
//! * [`toolset::FakeToolset`] — a `Toolset` with scriptable results, including hostile behaviour.
//! * [`audit`] — helpers for asserting on the audit trail.

pub mod audit;
pub mod scripted;
pub mod toolset;

/// The doubles and assertions most tests want.
pub mod prelude {
    pub use crate::audit::CapturedAudit;
    pub use crate::scripted::{RequestAssertions, ScriptedModel, Turn as ScriptedTurn};
    pub use crate::toolset::{FakeTool, FakeToolset};
}
