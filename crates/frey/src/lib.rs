//! **Frey** — a Rust agent framework where the context window is a managed resource.
//!
//! Most agent frameworks treat the prompt as a string you concatenate. Frey treats it as what it
//! actually is: a scarce, cache-sensitive, ordered resource with a budget and a price. Tools,
//! skills, and code-mode are three presentations of one progressively-disclosed catalog, and
//! nothing degrades quietly.
//!
//! ```
//! use frey::prelude::*;
//!
//! // Capabilities are per model, not per provider. The minimum cacheable prefix varies eightfold
//! // between models from one vendor, and getting it wrong means caching silently does nothing.
//! let opus = frey::profiles::opus5();
//! let haiku = frey::profiles::haiku45();
//! assert_eq!(opus.cache.min_prefix_tokens(), Some(512));
//! assert_eq!(haiku.cache.min_prefix_tokens(), Some(4_096));
//! ```
//!
//! # What is unusual about it
//!
//! **The cache planner refuses to waste your money.** It knows each provider's rules — Anthropic's
//! four breakpoints and per-model minimum prefix, OpenAI's automatic caching and routing key,
//! OpenRouter's per-upstream split — and it will not place a breakpoint on a segment that changed
//! last turn. When it cannot cache, it says why, in a sentence naming the culprit.
//!
//! **Untrusted data is a type.** Everything from outside is
//! [`Tainted`](frey_core::taint::Tainted), and passing it to something that needs trusted input is
//! a compile error. Raising integrity happens in one auditable place, records its call site, and is
//! usually done by a parser rather than a human.
//!
//! **Errors are typed by audience.** What the model is told, what the operator is told, and what a
//! user sees are three different fields. A tool failure can carry instructions the model can act on
//! instead of a bare refusal it will simply retry.
//!
//! **The journal is the session.** Every non-deterministic effect is recorded, so replay reproduces
//! a run exactly and diverges loudly at the first mismatch rather than quietly adapting.
//!
//! # Crate layout
//!
//! | Crate | What it does |
//! |---|---|
//! | [`frey_core`] | Types and traits. No I/O, so the planners are pure functions. |
//! | [`frey_context`] | Budget, cache planning, discovery, skills, code-mode codegen. |
//! | [`frey_providers`] | Anthropic, OpenAI Responses, OpenRouter, and config-defined dialects. |
//! | [`frey_tools`] | The layers every tool call passes through, and the `#[tool]` macro. |
//! | [`frey_agent`] | The loop, the journal, replay, and multi-agent spawning. |
//! | `frey_mcp` | Model Context Protocol, at the stateless `2026-07-28` revision. |
//! | `frey_sandbox` | Cross-platform confinement that fails closed. |
//! | `frey_a2a` | Agent-to-agent interoperability. |
//! | `frey_harness` | Sessions, approvals, AG-UI, and `doctor`. |

pub use frey_agent as agent;
pub use frey_context as context;
pub use frey_core as core;
pub use frey_providers as providers;
pub use frey_tools as tools;

#[cfg(feature = "a2a")]
pub use frey_a2a as a2a;
#[cfg(feature = "harness")]
pub use frey_harness as harness;
#[cfg(feature = "mcp")]
pub use frey_mcp as mcp;
#[cfg(feature = "sandbox")]
pub use frey_sandbox as sandbox;

pub use frey_context::profiles;
pub use frey_tools::tool;

/// Everything the common case needs, in one import.
///
/// Curated rather than a set of glob re-exports, because three names genuinely collide across
/// crates — `Request` means one thing to a provider and another to MCP, `ApprovalPolicy` exists at
/// both the tool and the harness layer, and `validate` is a verb two subsystems both need. Globbing
/// would make which one you got depend on import order. The colliding names are aliased here so
/// both remain reachable and neither is a surprise.
pub mod prelude {
    // Core vocabulary.
    pub use frey_core::capability::{
        Capability, Grant, GrantSet, HostPattern, PathScope, ProgramScope,
    };
    pub use frey_core::error::{
        InputRequest, ModelMessage, NeedsInput, Risk, ToolError, ToolErrorKind, ToolOutcome,
    };
    pub use frey_core::event::{Event, EventKind, Warning};
    pub use frey_core::ids::{AgentPath, CallId, ModelId, ProviderId, RunId, SessionId, ToolName};
    pub use frey_core::item::{Item, Role, Turn};
    pub use frey_core::provider::{ModelProvider, ProviderError, Response, StopReason};
    pub use frey_core::provider_caps::ProviderCapabilities;
    pub use frey_core::taint::{Tainted, Trusted, Untrusted, Validated};
    pub use frey_core::tool::{Invocation, Tool, ToolContent, ToolCx, Toolset};
    pub use frey_core::tool_def::{JsonSchema, ToolDefinition};
    pub use frey_core::usage::{Money, Usage};

    /// A request to a model. Aliased because MCP has a `Request` of its own.
    pub use frey_core::provider::Request as ModelRequest;

    // Context: the wedge.
    pub use frey_context::budget::{Budgeter, ContextBudget};
    pub use frey_context::cache::{CachePlan, CachePlanner, PreviousPrompt};
    pub use frey_context::codemode::{Strategy as CodeModeStrategy, generate_api};
    pub use frey_context::hash::{hash_parts, hash_text};
    pub use frey_context::search::{Bm25Search, RegexSearch};
    pub use frey_context::skills::{Skill, SkillIndexEntry, parse_skill};

    // Providers.
    #[cfg(feature = "http")]
    pub use frey_providers::HttpProvider;
    pub use frey_providers::anthropic::Anthropic;
    pub use frey_providers::dialect::{Auth, Dialect, DialectKind, ProviderConfig};
    pub use frey_providers::openai::OpenAiResponses;
    pub use frey_providers::openrouter::{OpenAiChat, OpenRouter};

    // Tools.
    pub use frey_tools::layer::{PolicyLayer, RedactLayer, TruncateLayer, risk_of};
    pub use frey_tools::registry::ToolRegistry;
    pub use frey_tools::tool;

    /// When a *tool call* needs approval. The harness has a session-level policy of the same name.
    pub use frey_tools::layer::ApprovalPolicy as ToolApprovalPolicy;

    // The loop.
    pub use frey_agent::journal::{Journal, Replay, ReplayError};
    pub use frey_agent::multi::{Child, Inheritance, Spawn, SpawnError, spawn};
    pub use frey_agent::run::{Agent, RunError, RunOutput, ToolHost};

    #[cfg(feature = "mcp")]
    pub use frey_mcp::client::{Catalog, CatalogCache, McpClient, McpError, Transport};

    #[cfg(feature = "sandbox")]
    pub use frey_core::sandbox::{ExecSpec, SandboxError, SandboxPolicy, SandboxReport};
    /// Check an exec request against a sandbox policy.
    #[cfg(feature = "sandbox")]
    pub use frey_sandbox::policy::validate as validate_exec;
    #[cfg(feature = "sandbox")]
    pub use frey_sandbox::policy::{Decision, decide};
    #[cfg(feature = "sandbox")]
    pub use frey_sandbox::probe::{LandlockAbi, backend_for_platform};

    #[cfg(feature = "harness")]
    pub use frey_harness::agui::{Frame, project, project_stream};
    #[cfg(feature = "harness")]
    pub use frey_harness::doctor::{Finding, Report, Severity};
    /// When a *session* requires approval. The tool layer has a per-call policy of the same name.
    #[cfg(feature = "harness")]
    pub use frey_harness::session::ApprovalPolicy as SessionApprovalPolicy;
    /// Check a harness configuration before anything runs.
    #[cfg(feature = "harness")]
    pub use frey_harness::session::validate as validate_harness;
    #[cfg(feature = "harness")]
    pub use frey_harness::session::{Session, Surface};

    #[cfg(feature = "a2a")]
    pub use frey_a2a::card::{AgentCard, AgentSkill};
    #[cfg(feature = "a2a")]
    pub use frey_a2a::task::{Task, TaskState, TaskStatus};
}

#[cfg(test)]
mod tests {
    use super::prelude::*;

    #[test]
    fn the_prelude_brings_in_what_the_common_case_needs() {
        // A smoke test for the facade: if a re-export breaks, this stops compiling, which is more
        // useful than discovering it from a user's bug report.
        let def = ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents",
            JsonSchema::empty_object(),
        );
        assert_eq!(risk_of(&def), Risk::Low);

        let mut registry = ToolRegistry::new();
        registry.register("native", def).unwrap();
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn the_cache_planner_is_reachable_from_the_prelude() {
        use frey_core::ids::SegmentId;
        use frey_core::segment::{Segment, SegmentKind, Stability};

        let segments = vec![Segment {
            id: SegmentId(0),
            kind: SegmentKind::Tools,
            stability: Stability::Static,
            hash: hash_text("tool definitions"),
            est_tokens: 12_000,
            label: "tools".into(),
        }];
        let plan =
            CachePlanner::plan(&segments, &PreviousPrompt::none(), &crate::profiles::opus5());
        assert!(plan.caches_anything());
    }

    #[test]
    fn untrusted_data_cannot_reach_a_trusted_sink_by_accident() {
        // The compile-fail proof lives in frey-core's UI tests; this asserts the runtime half is
        // reachable from the facade.
        let page: Untrusted<String> = Tainted::from_tool("http_get", "body".into());
        assert_eq!(page.label().0, frey_core::taint::IntegrityLevel::Low);
    }
}
