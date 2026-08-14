//! What a model provider must do, and what a failure from one means.
//!
//! Two traits, deliberately different shapes (ADR-0004):
//!
//! * [`ModelProvider`] — an endpoint that completes tokens. Frey owns the loop.
//! * [`AgentProvider`] — an external agent process that already owns its auth, tools, sandbox and
//!   loop. Frey hands it a task and consumes its events; it cannot be asked for raw completion.
//!
//! Keeping them apart is what makes the subscription story honest. Anthropic prohibit third-party
//! use of subscription OAuth, so "ride your existing plan" is implemented by delegating to the
//! vendor's own binary, which keeps its credentials inside its own process. Frey never stores,
//! mints, or replays a vendor subscription token, and no trait here gives it a place to.

// Trait methods here return `impl Future<..> + Send` rather than using `async fn`. The reason is
// concrete rather than stylistic: `async fn` in a trait leaves the future's auto traits unnameable,
// `dynosaur`'s erasure then boxes it as a plain `dyn Future`, and the agent loop cannot spawn the
// result. Writing the bound out fixes that, states the requirement in the public API, and makes
// `clippy::async_fn_in_trait` unnecessary. `erased_provider_futures_are_send` holds the line.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use smol_str::SmolStr;

use crate::ids::{AgentId, CallId, ModelId, ProviderId, ToolName};
use crate::item::{Item, Turn};
use crate::provider_caps::ProviderCapabilities;
use crate::segment::{CacheMark, CacheTtl};
use crate::tool_def::ToolDefinition;
use crate::usage::Usage;

/// How hard the model should think, where the provider exposes a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Let the provider decide.
    #[default]
    Default,
    /// Prefer speed.
    Low,
    /// Prefer quality.
    High,
}

/// One request to a model.
///
/// `extra` exists because provider nuance is the product: a knob Frey has not modelled yet must be
/// reachable without forking the adapter. Adapters pass it through verbatim and are expected to
/// document which keys they honour.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Which model.
    pub model: ModelId,
    /// The conversation so far.
    pub turns: Vec<Turn>,
    /// Tools visible this step, already filtered and ordered by the context engine.
    pub tools: Vec<ToolDefinition>,
    /// Where the cache planner wants breakpoints. Advisory: the adapter realises them in whatever
    /// form its provider accepts, or reports that it cannot.
    pub marks: Vec<CacheMark>,
    /// Cap on generated tokens.
    pub max_output: u32,
    /// How hard to think.
    pub effort: Effort,
    /// A stable key for cache-affinity routing, where the provider has one.
    pub cache_key: Option<SmolStr>,
    /// Provider-specific passthrough.
    pub extra: BTreeMap<SmolStr, serde_json::Value>,
}

/// Where a cache plan's marks land on the wire.
///
/// A [`CacheMark`] names a [`SegmentId`], and a dialect has no segment list — it has tools and
/// turns. This resolves one into the other, in **one place**, because the mapping is a contract
/// between the agent loop and every adapter and three copies of it would drift.
///
/// The contract: the tool block, when there is one, is segment 0; each turn is the segment after
/// it, in order. A mark naming a segment that does not exist is dropped rather than guessed at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkPlacement {
    /// A breakpoint at the end of the tool block.
    pub tools: Option<CacheTtl>,
    /// Breakpoints at the end of a turn, by index into `Request::turns`.
    pub turns: BTreeMap<usize, CacheTtl>,
}

impl MarkPlacement {
    /// How many marks actually landed somewhere.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.tools.is_some()) + self.turns.len()
    }

    /// Whether nothing landed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Request {
    /// Resolve this request's marks against its own tools and turns.
    ///
    /// Every dialect that realises breakpoints calls this rather than reading `marks` directly. It
    /// exists because the Anthropic adapter used to take `marks.last()` and put it on the last
    /// system block — collapsing a four-breakpoint Opus plan to one, dropping it entirely when the
    /// system prompt was empty, and never marking the tool block at all despite a doc comment three
    /// lines above saying that it did.
    #[must_use]
    pub fn mark_placement(&self) -> MarkPlacement {
        let has_tools = !self.tools.is_empty();
        let offset = usize::from(has_tools);
        let mut placement = MarkPlacement::default();
        for mark in &self.marks {
            let index = mark.at.index() as usize;
            if has_tools && index == 0 {
                placement.tools = Some(mark.ttl);
            } else if let Some(turn) = index.checked_sub(offset)
                && turn < self.turns.len()
            {
                placement.turns.insert(turn, mark.ttl);
            }
        }
        placement
    }
}

impl Default for ModelId {
    fn default() -> Self {
        Self::new("")
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The model wants one or more tools run.
    ToolUse,
    /// The output cap was reached. **Not** an end of turn: the answer is truncated.
    MaxTokens,
    /// A stop sequence matched.
    StopSequence,
    /// The model declined.
    Refusal,
    /// Something the adapter did not recognise, kept rather than flattened to `EndTurn`.
    Other(SmolStr),
}

impl StopReason {
    /// Whether the model is waiting on tool results.
    #[must_use]
    pub fn wants_tools(&self) -> bool {
        matches!(self, Self::ToolUse)
    }

    /// Whether the response is incomplete, so treating it as a final answer would be wrong.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::MaxTokens)
    }
}

/// One response from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The items produced, including anything preserved as `Opaque`.
    pub items: Vec<Item>,
    /// What it consumed.
    pub usage: Usage,
    /// Why it stopped.
    pub stop: StopReason,
    /// Which model actually served the request. Routers substitute models; the caller must be able
    /// to see when that happened, because it changes price, tokenizer, and cache behaviour.
    pub model: ModelId,
    /// Which provider actually served it, for the same reason.
    pub provider: ProviderId,
}

/// An incremental update while streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamEvent {
    /// A chunk of assistant text.
    TextDelta(String),
    /// A chunk of reasoning.
    ReasoningDelta(String),
    /// A tool call has started; arguments may still be arriving.
    ToolCallStarted {
        /// Correlation id.
        id: CallId,
        /// Which tool.
        name: ToolName,
    },
    /// A chunk of a tool call's JSON arguments.
    ToolArgsDelta {
        /// Correlation id.
        id: CallId,
        /// Partial JSON.
        json: String,
    },
    /// A complete item, for kinds that do not stream incrementally.
    Item(Box<Item>),
    /// The response is complete.
    Done(Box<Response>),
}

/// A boxed stream of streaming events.
pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send + 'static>>;

/// Why a provider call failed.
///
/// The classification exists to make one specific failure impossible to swallow: an OpenRouter 402
/// for exhausted credits returns quickly and looks transient, and a retry loop will happily grind
/// through an entire run turning every turn into a silent no-op. [`ProviderError::is_fatal`] is the
/// single place that decision is made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// Credentials are missing, wrong, or revoked. **Never retried.**
    #[error("authentication failed for {provider}: {detail}")]
    Auth {
        /// Which provider.
        provider: ProviderId,
        /// What it said.
        detail: String,
    },
    /// Out of credit, over a spend cap, or payment required. **Never retried.**
    #[error("billing failure for {provider}: {detail}")]
    Billing {
        /// Which provider.
        provider: ProviderId,
        /// What it said.
        detail: String,
    },
    /// Rate limited. Retryable, ideally after `retry_after_ms`.
    #[error("rate limited by {provider}")]
    RateLimit {
        /// Which provider.
        provider: ProviderId,
        /// How long the provider asked us to wait.
        retry_after_ms: Option<u64>,
    },
    /// The provider is overloaded. Retryable.
    #[error("{provider} is overloaded")]
    Overloaded {
        /// Which provider.
        provider: ProviderId,
    },
    /// Frey sent something the provider rejected. Not retryable: the same request will fail again.
    #[error("{provider} rejected the request: {detail}")]
    BadRequest {
        /// Which provider.
        provider: ProviderId,
        /// What it said.
        detail: String,
    },
    /// The response did not match the provider's documented shape.
    #[error("could not parse a response from {provider}: {detail}")]
    Protocol {
        /// Which provider.
        provider: ProviderId,
        /// What went wrong.
        detail: String,
    },
    /// Transport failure. Retryable.
    #[error("network failure talking to {provider}: {detail}")]
    Network {
        /// Which provider.
        provider: ProviderId,
        /// What went wrong.
        detail: String,
    },
    /// The caller asked for something this provider cannot do, and degrading was not acceptable.
    #[error("{provider} does not support {capability}")]
    Unsupported {
        /// Which provider.
        provider: ProviderId,
        /// What was wanted.
        capability: SmolStr,
    },
    /// The run was cancelled.
    #[error("cancelled")]
    Cancelled,
}

impl ProviderError {
    /// Whether retrying the identical request could plausibly succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimit { .. } | Self::Overloaded { .. } | Self::Network { .. })
    }

    /// Whether this failure should stop the run rather than be absorbed.
    ///
    /// Auth and billing are fatal on purpose. A run that quietly degrades to producing nothing is
    /// worse than a run that stops with a clear message.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Auth { .. } | Self::Billing { .. })
    }

    /// Classify an HTTP status from a provider.
    ///
    /// The mapping is deliberately explicit rather than "4xx bad, 5xx retry", because 402 and 429
    /// sit either side of the line that matters most.
    #[must_use]
    pub fn from_status(provider: &ProviderId, status: u16, detail: impl Into<String>) -> Self {
        let provider = provider.clone();
        let detail = detail.into();
        match status {
            401 | 403 => Self::Auth { provider, detail },
            402 => Self::Billing { provider, detail },
            429 => Self::RateLimit { provider, retry_after_ms: None },
            400 | 404 | 405 | 409 | 413 | 422 => Self::BadRequest { provider, detail },
            500..=504 | 529 => Self::Overloaded { provider },
            _ => Self::Protocol { provider, detail: format!("unexpected HTTP {status}: {detail}") },
        }
    }
}

/// An endpoint that completes tokens.
#[dynosaur::dynosaur(pub DynModelProvider = dyn(box) ModelProvider)]
pub trait ModelProvider: Send + Sync {
    /// Which adapter this is.
    fn id(&self) -> ProviderId;

    /// What this provider can do for a given model.
    ///
    /// Per model, not per provider: the minimum cacheable prefix varies eightfold between models
    /// from the same vendor.
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;

    /// Complete one request.
    ///
    /// # Errors
    /// Returns [`ProviderError`]; callers must check [`ProviderError::is_fatal`] before retrying.
    fn complete(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, ProviderError>> + Send;

    /// Complete one request, streaming.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if the stream could not be opened. Errors during the stream arrive
    /// as `Err` items.
    fn stream(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<EventStream, ProviderError>> + Send;
}

/// Sharing one adapter between many agents.
///
/// Every method here takes `&self`, which is what makes a single adapter usable from any number of
/// concurrent agents — but `Agent::new` takes its provider **by value**, so without this impl the
/// only way to give two agents the same provider is to construct two of them. That is precisely
/// what the adapter documentation warns against: each carries its own connection pool, DNS cache
/// and TLS session store, and multiplying those by a population fails as socket exhaustion rather
/// than as anything that looks like a client problem.
///
/// So the recommended pattern was documented in three places and did not compile. Found by the
/// first caller that actually needed thousands of agents on one pool.
impl<P: ModelProvider + ?Sized> ModelProvider for std::sync::Arc<P> {
    fn id(&self) -> ProviderId {
        (**self).id()
    }

    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities {
        (**self).capabilities(model)
    }

    fn complete(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, ProviderError>> + Send {
        (**self).complete(request)
    }

    fn stream(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<EventStream, ProviderError>> + Send {
        (**self).stream(request)
    }
}

/// A task handed to an external agent process.
#[derive(Debug, Clone)]
pub struct DelegatedTask {
    /// What to do.
    pub prompt: String,
    /// The directory the agent may work in.
    pub workspace: std::path::PathBuf,
    /// Tool names to allow, where the vendor supports restricting them.
    pub allowed_tools: Option<Vec<SmolStr>>,
    /// How long to wait before giving up.
    pub timeout_ms: u64,
}

/// Something an external agent reported.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentEvent {
    /// Text the agent produced.
    Text(String),
    /// The agent used a tool. Reported for display only: Frey did not mediate this call and did not
    /// sandbox it, and the audit record says so rather than implying otherwise.
    ToolUsed {
        /// The tool's name as the vendor spells it.
        name: SmolStr,
    },
    /// Usage the vendor reported, if any. Frey does not estimate another agent's spend.
    Usage(Box<Usage>),
    /// The agent finished.
    Finished {
        /// Whether it believes it succeeded.
        ok: bool,
    },
    /// The agent failed.
    Failed {
        /// What it said.
        detail: String,
    },
}

/// Delegation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DelegationError {
    /// The vendor's binary could not be found or started.
    #[error("could not start agent `{agent}`: {detail}")]
    Unavailable {
        /// Which agent.
        agent: AgentId,
        /// Why.
        detail: String,
    },
    /// The agent exceeded its time limit.
    #[error("agent `{agent}` timed out")]
    Timeout {
        /// Which agent.
        agent: AgentId,
    },
    /// The agent's output could not be parsed.
    #[error("could not parse output from agent `{agent}`: {detail}")]
    Protocol {
        /// Which agent.
        agent: AgentId,
        /// Why.
        detail: String,
    },
    /// The run was cancelled.
    #[error("cancelled")]
    Cancelled,
}

/// A boxed stream of events from a delegated agent.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'static>>;

/// An external agent process that owns its own auth, tools, sandbox, and loop.
///
/// Note what is missing: there is no method that returns tokens, no place to put a credential, and
/// no way to ask it to run a Frey tool. Delegation is all it offers, which is the point.
#[dynosaur::dynosaur(pub DynAgentProvider = dyn(box) AgentProvider)]
pub trait AgentProvider: Send + Sync {
    /// Which agent this is.
    fn id(&self) -> AgentId;

    /// Hand it a task and consume its events.
    ///
    /// # Errors
    /// Returns [`DelegationError`] if the agent could not be started or its output made no sense.
    fn delegate(
        &self,
        task: DelegatedTask,
    ) -> impl Future<Output = Result<AgentEventStream, DelegationError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> ProviderId {
        ProviderId::new("openrouter")
    }

    #[test]
    fn credit_exhaustion_is_fatal_and_never_retried() {
        // The failure this classification exists for: HTTP 402 returns fast and looks transient, so
        // a naive retry loop turns an entire run into silent no-ops while still charging for it.
        let err = ProviderError::from_status(&p(), 402, "insufficient credits");
        assert!(matches!(err, ProviderError::Billing { .. }));
        assert!(err.is_fatal());
        assert!(!err.is_retryable());
    }

    #[test]
    fn auth_failures_are_fatal_and_rate_limits_are_not() {
        for status in [401, 403] {
            let err = ProviderError::from_status(&p(), status, "nope");
            assert!(err.is_fatal(), "HTTP {status} must stop the run");
            assert!(!err.is_retryable());
        }
        let limited = ProviderError::from_status(&p(), 429, "slow down");
        assert!(!limited.is_fatal());
        assert!(limited.is_retryable());
    }

    #[test]
    fn bad_requests_are_neither_fatal_nor_retryable() {
        // Retrying an identical malformed request just wastes money; stopping the whole run is
        // also wrong, because the agent can often fix its own arguments and continue.
        let err = ProviderError::from_status(&p(), 400, "unknown field `cache_control`");
        assert!(!err.is_fatal());
        assert!(!err.is_retryable());
    }

    #[test]
    fn server_errors_are_retryable() {
        for status in [500, 502, 503, 529] {
            let err = ProviderError::from_status(&p(), status, "");
            assert!(err.is_retryable(), "HTTP {status} should be retried");
            assert!(!err.is_fatal());
        }
    }

    #[test]
    fn unrecognised_statuses_are_reported_rather_than_guessed_at() {
        let err = ProviderError::from_status(&p(), 418, "teapot");
        assert!(matches!(err, ProviderError::Protocol { .. }));
        assert!(format!("{err}").contains("418"), "the status must survive into the message");
    }

    #[test]
    fn truncation_is_distinguishable_from_a_finished_turn() {
        assert!(StopReason::MaxTokens.is_truncated());
        assert!(!StopReason::EndTurn.is_truncated());
        assert!(StopReason::ToolUse.wants_tools());
        // An unknown reason is preserved rather than flattened into EndTurn, which would make a
        // truncated or refused answer look complete.
        let other = StopReason::Other("content_filter".into());
        assert!(!other.is_truncated() && !other.wants_tools());
        assert_eq!(serde_json::to_string(&other).unwrap(), r#"{"other":"content_filter"}"#);
    }

    #[test]
    fn one_adapter_behind_an_arc_serves_many_agents() {
        // The pattern the adapter docs recommend, and until this impl existed it did not compile:
        // `Agent::new` takes its provider by value, so sharing one required `Arc<P>` to itself be a
        // provider. A caller following the documentation got a trait-bound error and the obvious
        // way out — construct one adapter each — is the failure mode the documentation is warning
        // about. Written as a compile-time proof because that is the half that was missing.
        struct Stub;
        impl ModelProvider for Stub {
            fn id(&self) -> ProviderId {
                ProviderId::new("stub")
            }
            fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
                ProviderCapabilities::minimal(1_000, 100)
            }
            async fn complete(&self, _request: Request) -> Result<Response, ProviderError> {
                Err(ProviderError::Cancelled)
            }
            async fn stream(&self, _request: Request) -> Result<EventStream, ProviderError> {
                Err(ProviderError::Cancelled)
            }
        }

        fn takes_by_value<P: ModelProvider>(provider: P) -> ProviderId {
            provider.id()
        }

        let shared = std::sync::Arc::new(Stub);
        // Two owners, one adapter, one connection pool.
        assert_eq!(takes_by_value(std::sync::Arc::clone(&shared)), ProviderId::new("stub"));
        assert_eq!(takes_by_value(std::sync::Arc::clone(&shared)), ProviderId::new("stub"));
        assert_eq!(shared.capabilities(&ModelId::new("any")).max_context, 1_000);
    }

    #[test]
    fn provider_traits_are_object_safe_through_dynosaur() {
        // Proves the erasure compiles. The registry stores `Box<DynModelProvider>`, so if this
        // stops working the whole provider layer stops working.
        struct Stub;
        impl ModelProvider for Stub {
            fn id(&self) -> ProviderId {
                ProviderId::new("stub")
            }
            fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
                ProviderCapabilities::minimal(1_000, 100)
            }
            async fn complete(&self, _request: Request) -> Result<Response, ProviderError> {
                Err(ProviderError::Cancelled)
            }
            async fn stream(&self, _request: Request) -> Result<EventStream, ProviderError> {
                Err(ProviderError::Cancelled)
            }
        }

        let erased: Box<DynModelProvider<'static>> = DynModelProvider::new_box(Stub);
        assert_eq!(erased.id(), ProviderId::new("stub"));
        assert_eq!(erased.capabilities(&ModelId::new("any")).max_context, 1_000);
    }

    #[test]
    fn erased_provider_futures_are_send() {
        // The property the agent loop actually needs: a call on an erased provider must be
        // spawnable onto another thread. If `dynosaur` ever stopped producing `Send` futures, the
        // whole multi-agent story would quietly stop working, so it is asserted rather than assumed.
        fn assert_send<T: Send>(_: T) {}
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}

        struct Stub;
        impl ModelProvider for Stub {
            fn id(&self) -> ProviderId {
                ProviderId::new("stub")
            }
            fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
                ProviderCapabilities::minimal(1_000, 100)
            }
            async fn complete(&self, _request: Request) -> Result<Response, ProviderError> {
                Err(ProviderError::Cancelled)
            }
            async fn stream(&self, _request: Request) -> Result<EventStream, ProviderError> {
                Err(ProviderError::Cancelled)
            }
        }

        assert_send_sync::<DynModelProvider<'static>>();
        let erased: Box<DynModelProvider<'static>> = DynModelProvider::new_box(Stub);
        assert_send(async move { erased.complete(Request::default()).await });
    }

    #[test]
    fn an_agent_provider_cannot_be_asked_for_tokens() {
        // Enforced by the trait's shape rather than by documentation: `AgentProvider` has no
        // completion method, so no adapter can accidentally grow one.
        struct Stub;
        impl AgentProvider for Stub {
            fn id(&self) -> AgentId {
                AgentId::new("claude")
            }
            async fn delegate(
                &self,
                _task: DelegatedTask,
            ) -> Result<AgentEventStream, DelegationError> {
                Err(DelegationError::Cancelled)
            }
        }
        let erased: Box<DynAgentProvider<'static>> = DynAgentProvider::new_box(Stub);
        assert_eq!(erased.id(), AgentId::new("claude"));
    }
}
