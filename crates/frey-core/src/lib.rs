//! Core domain types for the [Frey](https://github.com/newsbubbles/frey) agent framework.
//!
//! This crate holds the vocabulary every other Frey crate speaks. It performs **no I/O** and takes
//! no runtime dependency, which is what lets the cache planner, the budgeter, and the policy engine
//! be pure functions with exhaustive unit tests rather than integration tests against a live API.
//!
//! # Where to start
//!
//! | Module | What lives there |
//! |---|---|
//! | [`taint`] | The information-flow lattice. Everything from outside is [`taint::Tainted`]. |
//! | [`audit`] | The trail every security-relevant decision writes to. |
//! | [`error`] | Failures typed by *audience*: model, operator, and user are three different readers. |
//! | [`ids`] | Newtype identifiers. Never random, so replay is deterministic. |
//! | [`item`] | The conversation model — items, not messages. |
//! | [`capability`] | What an agent may reach, and the Rule of Two. |
//! | [`tool_def`] | How a tool is described and presented. |
//! | [`provider_caps`] | What a provider can actually do, so degradation is explicit. |
//! | [`segment`] | Prompt segments and cache marks. |
//! | [`usage`] | Tokens and money, kept honest. |
//! | [`event`] | The one event stream that feeds AG-UI, A2A, OpenTelemetry, and the journal. |
//! | [`provider`] | The two provider contracts, kept deliberately different shapes. |
//! | [`tool`] | The tool and toolset contracts, and capability search. |
//! | [`sandbox`] | What a sandbox must promise, and the audit artefact every backend produces. |

pub mod audit;
pub mod capability;
pub mod error;
pub mod event;
pub mod ids;
pub mod item;
pub mod provider;
pub mod provider_caps;
pub mod sandbox;
pub mod segment;
pub mod taint;
pub mod tool;
pub mod tool_def;
pub mod usage;
pub mod validate;

/// The types most callers want.
pub mod prelude {
    pub use crate::audit::{Declassification, Endorsement};
    pub use crate::capability::{
        Capability, Grant, GrantSet, HostPattern, PathScope, ProgramScope,
    };
    pub use crate::error::{
        InputRequest, ModelMessage, NeedsInput, RetryDirective, Risk, ToolError, ToolErrorKind,
        ToolOutcome,
    };
    pub use crate::event::{Event, EventKind, Warning};
    pub use crate::ids::{AgentPath, CallId, ModelId, ProviderId, RunId, SessionId, ToolName};
    pub use crate::item::{Item, Role, Turn};
    pub use crate::provider::{
        AgentProvider, ModelProvider, ProviderError, Request, Response, StopReason,
    };
    pub use crate::provider_caps::ProviderCapabilities;
    pub use crate::sandbox::{ExecSpec, SandboxBackend, SandboxPolicy, SandboxReport};
    pub use crate::taint::{Tainted, Trusted, Untrusted, Validated};
    pub use crate::tool::{Invocation, Tool, ToolContent, ToolCx, ToolValue, Toolset};
    pub use crate::tool_def::{JsonSchema, ToolDefinition};
    pub use crate::usage::{Money, Usage};
}
