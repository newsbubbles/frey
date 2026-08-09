//! What a tool is, and what a collection of tools is.
//!
//! Native functions, MCP servers, skill scripts, sub-agents, and remote A2A peers are all
//! [`Tool`] implementations. That is what makes swapping an MCP server for a local function a
//! one-line change, and — more importantly — what makes the policy, approval, sandbox, and audit
//! layers unavoidable rather than opt-in: there is no second path to executing anything.
//!
//! A [`Toolset`] is asked for its definitions **once per step**, not once at startup, because
//! visibility is a function of the current task, the remaining budget, and the current policy.

// Trait methods here return `impl Future<..> + Send` rather than using `async fn`. The reason is
// concrete rather than stylistic: `async fn` in a trait leaves the future's auto traits unnameable,
// `dynosaur`'s erasure then boxes it as a plain `dyn Future`, and the agent loop cannot spawn the
// result. Writing the bound out fixes that and states the requirement in the public API.
// `provider::tests::erased_provider_futures_are_send` holds the line.
use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use smol_str::SmolStr;

use crate::capability::GrantSet;
use crate::error::ToolOutcome;
use crate::ids::{CallId, RunId, SessionId, ToolName};
use crate::item::Caller;
use crate::taint::{Provenance, Untrusted};
use crate::tool_def::ToolDefinition;

/// What a tool produces on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContent {
    /// The rendering the model sees.
    pub text: String,
    /// Machine-readable output, when the tool has an output schema.
    pub structured: Option<serde_json::Value>,
    /// How many bytes were withheld. Non-zero obliges the caller to tell the model how to get the
    /// rest; silent truncation is the bug this field exists to prevent.
    pub bytes_elided: u64,
}

impl ToolContent {
    /// Plain text output, complete.
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), structured: None, bytes_elided: 0 }
    }

    /// Attach machine-readable output.
    #[must_use]
    pub fn with_structured(mut self, value: serde_json::Value) -> Self {
        self.structured = Some(value);
        self
    }

    /// Record that output was withheld.
    #[must_use]
    pub fn elided(mut self, bytes: u64) -> Self {
        self.bytes_elided = bytes;
        self
    }
}

/// A tool's result, labelled. Tool output is attacker-controlled by construction, so the label is
/// applied at the boundary and tool authors never write one themselves.
pub type ToolValue = Untrusted<ToolContent>;

/// One request to run a tool.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// Correlation id.
    pub id: CallId,
    /// Which tool.
    pub name: ToolName,
    /// The arguments the model produced. Unvalidated: `StrictSupport` is not a guarantee on most
    /// providers, so the tool layer validates before dispatch.
    pub args: serde_json::Value,
    /// How the call was made. Enforced client-side, because the provider's own `allowed_callers`
    /// is documented as guidance rather than a security boundary.
    pub caller: Caller,
}

/// Everything a tool may know about the run it is part of.
///
/// Note what is absent: there is no ambient filesystem handle, no HTTP client, and no environment.
/// A tool reaches the world only through capabilities it declared, which the runtime resolves.
#[derive(Debug, Clone)]
pub struct ToolCx {
    /// Which run.
    pub run: RunId,
    /// Which session.
    pub session: SessionId,
    /// What this tool is permitted to do.
    pub grants: GrantSet,
    /// Where results from this tool should say they came from.
    pub provenance: Provenance,
    /// Answers to a previous [`ToolOutcome::NeedsInput`], when this call is a retry.
    ///
    /// The multi round-trip pattern replaced server-initiated requests in MCP `2026-07-28`, and it
    /// is what makes a stateless server possible: rather than calling back to the client, a tool
    /// returns what it needs, and the client **re-sends the original call** with the answers
    /// attached. Nothing was remembered in between, so the answers have to arrive here.
    ///
    /// `None` means this is a first attempt. A tool that returns `NeedsInput` and is then called
    /// again with `None` should return `NeedsInput` again rather than assuming approval — an
    /// absent answer is not a yes.
    ///
    /// Shaped as a raw value rather than a typed enum because the payload is defined by whatever
    /// asked for it, and A2A, AG-UI and MCP each spell their answers differently.
    pub resume: Option<Resume>,
}

/// Answers carried by a retry, plus the state the tool sealed for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    /// What the tool sealed on the way out, returned untouched.
    ///
    /// Opaque to the client by contract. A client that edits it is forging the tool's own memory,
    /// which is precisely the attack a stateless resume token has to survive.
    pub state: serde_json::Value,
    /// The answers, in the order the requests were made.
    pub answers: Vec<serde_json::Value>,
}

impl ToolCx {
    /// Whether `capability` is granted.
    #[must_use]
    pub fn permits(&self, capability: &crate::capability::Capability) -> bool {
        self.grants.permits(capability)
    }

    /// A context for a first attempt, with no resume payload.
    ///
    /// Exists so that adding a field here does not break every construction site, and so the common
    /// case reads as what it is.
    #[must_use]
    pub fn new(run: RunId, session: SessionId, grants: GrantSet, provenance: Provenance) -> Self {
        Self { run, session, grants, provenance, resume: None }
    }

    /// The same context, carrying answers to an earlier request for input.
    #[must_use]
    pub fn resuming(mut self, resume: Resume) -> Self {
        self.resume = Some(resume);
        self
    }
}

/// What a toolset knows when deciding what to expose this step.
#[derive(Debug, Clone)]
pub struct StepCx {
    /// Which run.
    pub run: RunId,
    /// Which session.
    pub session: SessionId,
    /// What the agent is currently trying to do, for relevance scoring.
    pub task: String,
    /// Tokens still available for tool definitions.
    pub tokens_available: u32,
}

/// A toolset could not be consulted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ToolsetError {
    /// The backing service was unreachable.
    #[error("toolset `{name}` is unavailable: {detail}")]
    Unavailable {
        /// Which toolset.
        name: SmolStr,
        /// Why.
        detail: String,
    },
    /// The toolset returned something unusable.
    #[error("toolset `{name}` returned an invalid definition: {detail}")]
    Invalid {
        /// Which toolset.
        name: SmolStr,
        /// Why.
        detail: String,
    },
}

/// Something the agent can do.
#[dynosaur::dynosaur(pub DynTool = dyn(box) Tool)]
pub trait Tool: Send + Sync {
    /// How this tool is described and presented.
    fn definition(&self) -> &ToolDefinition;

    /// Run it.
    ///
    /// Returns [`ToolOutcome`] rather than `Result`, because a failure is a *value* the model must
    /// see and reason about, not an exception that unwinds past it.
    fn call(
        &self,
        invocation: Invocation,
        cx: &ToolCx,
    ) -> impl Future<Output = ToolOutcome<ToolValue>> + Send;
}

/// A collection of tools that may change between steps.
#[dynosaur::dynosaur(pub DynToolset = dyn(box) Toolset)]
pub trait Toolset: Send + Sync {
    /// A name for diagnostics.
    fn name(&self) -> SmolStr;

    /// What this toolset exposes right now.
    ///
    /// # Errors
    /// Returns [`ToolsetError`] when the backing service cannot be consulted.
    fn definitions(
        &self,
        cx: &StepCx,
    ) -> impl Future<Output = Result<Vec<ToolDefinition>, ToolsetError>> + Send;

    /// Run one of its tools.
    fn call(
        &self,
        invocation: Invocation,
        cx: &ToolCx,
    ) -> impl Future<Output = ToolOutcome<ToolValue>> + Send;

    /// Guidance to add to the prompt when any of this toolset's tools are visible.
    fn instructions(&self, _cx: &StepCx) -> Option<String> {
        None
    }
}

/// How a capability was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SearchKind {
    /// A regular expression over names and descriptions. Mirrors Anthropic's regex variant, down to
    /// the 200-character pattern limit, so emulation and delegation behave the same.
    Regex,
    /// Lexical ranking over the catalog.
    Bm25,
    /// Vector similarity.
    Embedding,
    /// The provider searched its own catalog server-side.
    ProviderNative,
}

/// A request to find capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// What the model asked for: a regex, or natural language, depending on the search kind.
    pub text: String,
    /// How many results to return. Defaults to five, matching the provider-native implementations
    /// so that swapping between them does not change the model's experience.
    pub limit: u8,
}

impl SearchQuery {
    /// A query with the default limit.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), limit: 5 }
    }
}

/// A capability the search found.
///
/// The score is an integer in basis points rather than a float, so hits compare and sort exactly.
/// Search results end up in the journal, and replay asserts on them; a `f32` would make two
/// identical runs occasionally disagree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchHit {
    /// Which tool.
    pub name: ToolName,
    /// How well it matched, `0..=10_000`.
    pub score_bp: u16,
}

impl SearchHit {
    /// A hit with a score expressed as a fraction in `0.0..=1.0`.
    #[must_use]
    pub fn new(name: ToolName, score: f64) -> Self {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let score_bp = (score.clamp(0.0, 1.0) * 10_000.0).round() as u16;
        Self { name, score_bp }
    }

    /// The score as a fraction in `0.0..=1.0`.
    #[must_use]
    pub fn score(&self) -> f64 {
        f64::from(self.score_bp) / 10_000.0
    }
}

/// Searching failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// The query was malformed, e.g. an invalid regex or one over the length limit.
    #[error("invalid search query: {0}")]
    InvalidQuery(String),
    /// The index was not ready.
    #[error("the capability index is unavailable: {0}")]
    Unavailable(String),
}

/// Finds capabilities in a catalog too large to show the model all at once.
#[dynosaur::dynosaur(pub DynCapabilitySearch = dyn(box) CapabilitySearch)]
pub trait CapabilitySearch: Send + Sync {
    /// Which strategy this is.
    fn kind(&self) -> SearchKind;

    /// Find capabilities matching `query`.
    ///
    /// # Errors
    /// Returns [`SearchError`] for a malformed query or an unavailable index. An empty result is
    /// success, not an error — the model needs to learn that nothing matched.
    fn search(
        &self,
        query: &SearchQuery,
        cx: &StepCx,
    ) -> impl Future<Output = Result<Vec<SearchHit>, SearchError>> + Send;
}

/// A stream of output from a running sandboxed process.
pub type OutputStream = Pin<Box<dyn Stream<Item = Vec<u8>> + Send + 'static>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, Grant, PathScope};
    use crate::error::{ToolError, ToolErrorKind};
    use crate::tool_def::JsonSchema;

    fn cx() -> ToolCx {
        ToolCx {
            run: RunId::new("r1"),
            session: SessionId::new("s1"),
            grants: GrantSet::new([Grant::operator(Capability::FsRead(
                PathScope::new(["./src"]).unwrap(),
            ))]),
            provenance: Provenance::new("tool:fs_read"),
            resume: None,
        }
    }

    struct Echo {
        def: ToolDefinition,
    }

    impl Tool for Echo {
        fn definition(&self) -> &ToolDefinition {
            &self.def
        }

        async fn call(&self, invocation: Invocation, cx: &ToolCx) -> ToolOutcome<ToolValue> {
            let wanted = Capability::FsRead(PathScope::new(["./src"]).unwrap());
            if !cx.permits(&wanted) {
                return ToolOutcome::Denied(
                    ToolError::new(ToolErrorKind::Denied, "reading ./src is not permitted")
                        .guide("Ask the operator to widen fs:read, or work under a granted path."),
                );
            }
            ToolOutcome::Ok(Untrusted::with_provenance(
                ToolContent::text(invocation.args.to_string()),
                cx.provenance.clone(),
            ))
        }
    }

    fn echo() -> Echo {
        Echo {
            def: ToolDefinition::new(
                "fs_read",
                "Read a file from the workspace and return its contents",
                JsonSchema::empty_object(),
            ),
        }
    }

    fn invocation() -> Invocation {
        Invocation {
            id: CallId::new("c1"),
            name: ToolName::new("fs_read"),
            args: serde_json::json!({"path": "src/main.rs"}),
            caller: Caller::Direct,
        }
    }

    #[test]
    fn tool_traits_are_object_safe_through_dynosaur() {
        let erased: Box<DynTool<'static>> = DynTool::new_box(echo());
        assert_eq!(erased.definition().name, ToolName::new("fs_read"));
    }

    #[test]
    fn a_tool_result_is_untrusted_by_construction() {
        let outcome = pollster::block_on(echo().call(invocation(), &cx()));
        let ToolOutcome::Ok(value) = outcome else { panic!("expected success") };
        assert_eq!(value.label().0, crate::taint::IntegrityLevel::Low);
        assert_eq!(value.provenance().origin.as_str(), "tool:fs_read");
    }

    #[test]
    fn a_tool_without_its_capability_is_denied_and_told_why() {
        let bare = ToolCx { grants: GrantSet::empty(), ..cx() };
        let outcome = pollster::block_on(echo().call(invocation(), &bare));
        let ToolOutcome::Denied(err) = &outcome else { panic!("expected a denial") };
        assert_eq!(err.kind(), ToolErrorKind::Denied);
        assert!(
            err.model().guidance.as_deref().unwrap().contains("fs:read"),
            "a denial the model cannot act on just causes a retry loop"
        );
    }

    #[test]
    fn search_queries_default_to_the_provider_native_result_limit() {
        // Five, matching Anthropic's server-side default, so that emulated and delegated discovery
        // give the model the same experience.
        assert_eq!(SearchQuery::new("weather").limit, 5);
    }
}
