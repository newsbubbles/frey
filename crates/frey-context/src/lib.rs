//! The Frey context engine: what the model sees, in what order, at what price.
//!
//! Everything here is a **pure function**. Given the same segments, the same previous turn, and the
//! same provider capabilities, the planner produces the same plan — no I/O, no clock, no
//! randomness. That is what makes provider quirks unit-testable instead of production surprises,
//! and it is why this milestone lands before the first HTTP request exists.
//!
//! ```
//! use frey_context::{cache::{CachePlanner, PreviousPrompt}, hash::hash_text, profiles};
//! use frey_core::ids::SegmentId;
//! use frey_core::segment::{Segment, SegmentKind, Stability};
//!
//! let segments = vec![
//!     Segment {
//!         id: SegmentId(0),
//!         kind: SegmentKind::Tools,
//!         stability: Stability::Static,
//!         hash: hash_text("tool definitions"),
//!         est_tokens: 12_000,
//!         label: "tools".into(),
//!     },
//!     Segment {
//!         id: SegmentId(1),
//!         kind: SegmentKind::History,
//!         stability: Stability::Volatile,
//!         hash: hash_text("what is the weather?"),
//!         est_tokens: 20,
//!         label: "history".into(),
//!     },
//! ];
//!
//! let plan = CachePlanner::plan(&segments, &PreviousPrompt::none(), &profiles::opus5());
//! assert_eq!(plan.marks.len(), 1);
//! assert_eq!(plan.marks[0].at, SegmentId(0)); // after the tools, before the question
//! ```

pub mod budget;
pub mod cache;
pub mod codemode;
pub mod hash;
pub mod profiles;
pub mod search;
pub mod skills;

/// The types most callers want.
pub mod prelude {
    pub use crate::budget::{Budgeter, ContextBudget, Floors};
    pub use crate::cache::{CachePlan, CachePlanner, PreviousPrompt};
    pub use crate::codemode::{Strategy, generate_api};
    pub use crate::hash::{hash_parts, hash_text};
    pub use crate::search::{Bm25Search, RegexSearch};
    pub use crate::skills::{Skill, SkillIndexEntry, parse_skill};
}
