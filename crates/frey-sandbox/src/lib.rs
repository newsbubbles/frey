//! Process confinement for [Frey](https://github.com/newsbubbles/frey).
//!
//! Two rules separate a security feature from security theatre, and both are enforced here rather
//! than documented and hoped for:
//!
//! 1. **Fail closed.** If the requested policy cannot be enforced, running is an error naming the
//!    missing controls — not a warning and a process. Running unconfined *because confinement was
//!    unavailable* is the failure mode that makes an audit go badly.
//! 2. **Report what was enforced, not what was asked.** [`frey_core::sandbox::SandboxReport`] is
//!    populated from what the platform actually did, and a degraded run never looks like a clean one.
//!
//! The parts that decide *whether* a run is allowed and *what it is told* are pure functions, so
//! the degraded and denied paths — the ones that matter and that a healthy CI machine cannot
//! reproduce — are ordinary unit tests.

pub mod policy;
pub mod probe;

/// The types most callers want.
pub mod prelude {
    pub use crate::policy::{Decision, allow_degraded, decide, validate};
    pub use crate::probe::{LandlockAbi, backend_for_platform, linux_availability};
    pub use frey_core::sandbox::{
        Availability, BackendId, Control, ExecSpec, SandboxError, SandboxPolicy, SandboxReport,
    };
}
