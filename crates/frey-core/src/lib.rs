//! Core domain types for the [Frey](https://github.com/newsbubbles/frey) agent framework.
//!
//! This crate holds the vocabulary every other Frey crate speaks: information-flow labels,
//! capabilities, the error model, and (shortly) the conversation item model. It performs **no I/O**
//! and takes no runtime dependency, which is what makes the planner, the policy engine, and the
//! cache planner unit-testable as pure functions.
//!
//! # Where to start
//!
//! * [`taint`] — the label lattice. Everything from outside is [`taint::Tainted`].
//! * [`audit`] — the trail that every security-relevant decision writes to.
//! * [`error`] — failures typed by *audience*: what the model sees, what the operator sees, and
//!   what a human user sees are three different things.

pub mod audit;
pub mod error;
pub mod taint;

/// The types most callers want.
pub mod prelude {
    pub use crate::audit::{Declassification, Endorsement};
    pub use crate::error::{ModelMessage, RetryDirective, ToolError, ToolErrorKind, ToolOutcome};
    pub use crate::taint::{Tainted, Trusted, Untrusted, Validated};
}
