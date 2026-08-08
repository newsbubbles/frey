//! What a provider can actually do.
//!
//! This is the anti-lying mechanism. Every framework that presents one uniform interface over
//! several providers has to decide what to do when a feature is missing, and the two common answers
//! — pretend it worked, or crash — are both wrong. Frey's answer is that the agent **asks**, then
//! degrades explicitly and visibly, emitting a warning that names the capability and the fallback.
//!
//! Capabilities are per `(provider, model)`, not per provider: minimum cacheable prefix length
//! varies by model within a single vendor, by a factor of eight.

use crate::segment::CacheTtl;

/// Whether the provider can search a tool catalog server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolSearchSupport {
    /// The provider searches deferred tools itself.
    Native {
        /// How many tools one search returns.
        max_results: u8,
        /// How many tools may be deferred in a single request.
        max_deferred: u32,
    },
    /// No server-side search. Frey searches locally and injects definitions after the cache
    /// breakpoint, which is what the native implementations do internally anyway.
    #[default]
    None,
}

impl ToolSearchSupport {
    /// Whether discovery should be delegated to the provider.
    #[must_use]
    pub fn is_native(self) -> bool {
        matches!(self, Self::Native { .. })
    }
}

/// How a provider caches prompt prefixes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheSupport {
    /// The caller places breakpoints. Anthropic, and Anthropic-shaped models routed elsewhere.
    Explicit {
        /// How many breakpoints one request may carry. Anthropic allow four, one of which is
        /// consumed if automatic caching is also enabled.
        max_breakpoints: u8,
        /// Which lifetimes are available.
        ttls: Vec<CacheTtl>,
        /// The shortest prefix that will actually be cached. Below this, caching silently does
        /// nothing and no error is returned — which is exactly why Frey warns instead.
        min_prefix_tokens: u32,
    },
    /// The provider caches automatically above a threshold. OpenAI, and most open models.
    Automatic {
        /// The threshold.
        min_prefix_tokens: u32,
        /// Whether the caller may switch to explicit breakpoints.
        explicit_available: bool,
    },
    /// No prompt caching.
    None,
}

impl CacheSupport {
    /// The minimum prefix that will be cached, if caching exists at all.
    #[must_use]
    pub fn min_prefix_tokens(&self) -> Option<u32> {
        match self {
            Self::Explicit { min_prefix_tokens, .. }
            | Self::Automatic { min_prefix_tokens, .. } => Some(*min_prefix_tokens),
            Self::None => None,
        }
    }

    /// How many breakpoints the planner may place.
    #[must_use]
    pub fn breakpoint_budget(&self) -> u8 {
        match self {
            Self::Explicit { max_breakpoints, .. } => *max_breakpoints,
            Self::Automatic { explicit_available: true, .. } => 1,
            _ => 0,
        }
    }

    /// Whether a given lifetime is available.
    #[must_use]
    pub fn supports_ttl(&self, ttl: CacheTtl) -> bool {
        match self {
            Self::Explicit { ttls, .. } => ttls.contains(&ttl),
            Self::Automatic { .. } => ttl == CacheTtl::Short,
            Self::None => false,
        }
    }
}

/// What happens to the model's reasoning between turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSupport {
    /// The model does not expose reasoning.
    #[default]
    None,
    /// Reasoning is returned in the clear and may be replayed.
    Plain,
    /// Reasoning comes back encrypted and **must** be replayed verbatim, or the model loses its
    /// chain of thought and you pay to regenerate it.
    Encrypted,
}

impl ReasoningSupport {
    /// Whether dropping a reasoning item would lose state the provider expects back.
    #[must_use]
    pub fn requires_round_trip(self) -> bool {
        matches!(self, Self::Encrypted)
    }
}

/// Whether tool arguments are constrained to the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrictSupport {
    /// Arguments are grammar-constrained and always match.
    Always,
    /// The provider tries, and silently falls back to unconstrained if the schema cannot be
    /// compiled. Frey must validate the arguments itself and produce a model-directed error.
    Attempted,
    /// No constraint at all.
    #[default]
    None,
}

impl StrictSupport {
    /// Whether Frey must validate tool arguments itself before dispatching.
    #[must_use]
    pub fn needs_client_validation(self) -> bool {
        !matches!(self, Self::Always)
    }
}

/// An input or output modality.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Modality {
    /// Text.
    Text,
    /// Still images.
    Image,
    /// Audio.
    Audio,
    /// Documents such as PDFs.
    Document,
    /// Video.
    Video,
}

/// Everything the agent loop needs to know before it renders a request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderCapabilities {
    /// Server-side tool search.
    pub tool_search: ToolSearchSupport,
    /// Whether the provider can call tools from inside its own code sandbox.
    pub programmatic_tool_calling: bool,
    /// Prompt caching.
    pub cache: CacheSupport,
    /// Reasoning round-tripping.
    pub reasoning: ReasoningSupport,
    /// Schema enforcement.
    pub strict_schema: StrictSupport,
    /// Whether several tool calls may be returned in one turn.
    pub parallel_tool_calls: bool,
    /// Accepted input modalities.
    pub input_modalities: Vec<Modality>,
    /// Produced output modalities.
    pub output_modalities: Vec<Modality>,
    /// Context window, in tokens.
    pub max_context: u32,
    /// Maximum tokens in one response.
    pub max_output: u32,
    /// Whether the provider reports what a call cost. When false, the ledger will have gaps and any
    /// figure Frey shows is an estimate.
    pub reports_cost: bool,
}

impl ProviderCapabilities {
    /// A deliberately pessimistic baseline: text only, no caching, no search, nothing clever.
    ///
    /// Config-defined providers start here, so an unspecified capability is *absent* rather than
    /// assumed. Claiming a feature you do not have is worse than not having it.
    #[must_use]
    pub fn minimal(max_context: u32, max_output: u32) -> Self {
        Self {
            tool_search: ToolSearchSupport::None,
            programmatic_tool_calling: false,
            cache: CacheSupport::None,
            reasoning: ReasoningSupport::None,
            strict_schema: StrictSupport::None,
            parallel_tool_calls: false,
            input_modalities: vec![Modality::Text],
            output_modalities: vec![Modality::Text],
            max_context,
            max_output,
            reports_cost: false,
        }
    }

    /// Whether an input modality is accepted.
    #[must_use]
    pub fn accepts(&self, modality: Modality) -> bool {
        self.input_modalities.contains(&modality)
    }

    /// How much room the prompt has, once the response reservation is set aside.
    #[must_use]
    pub fn prompt_budget(&self, reserve_output: u32) -> u32 {
        self.max_context.saturating_sub(reserve_output.min(self.max_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_opus5() -> ProviderCapabilities {
        ProviderCapabilities {
            tool_search: ToolSearchSupport::Native { max_results: 5, max_deferred: 10_000 },
            programmatic_tool_calling: true,
            cache: CacheSupport::Explicit {
                max_breakpoints: 4,
                ttls: vec![CacheTtl::Short, CacheTtl::Long],
                min_prefix_tokens: 512,
            },
            reasoning: ReasoningSupport::Plain,
            strict_schema: StrictSupport::Always,
            parallel_tool_calls: true,
            input_modalities: vec![Modality::Text, Modality::Image, Modality::Document],
            output_modalities: vec![Modality::Text],
            max_context: 200_000,
            max_output: 64_000,
            reports_cost: false,
        }
    }

    #[test]
    fn the_default_is_pessimistic_so_absent_features_are_never_assumed() {
        let caps = ProviderCapabilities::minimal(8_192, 2_048);
        assert!(!caps.tool_search.is_native());
        assert!(!caps.programmatic_tool_calling);
        assert_eq!(caps.cache.breakpoint_budget(), 0);
        assert_eq!(caps.cache.min_prefix_tokens(), None);
        assert!(!caps.reports_cost, "an unspecified provider does not report cost");
        assert!(!caps.accepts(Modality::Image));
    }

    #[test]
    fn minimum_cacheable_prefix_varies_by_model_within_one_vendor() {
        // Real numbers: Opus 5 caches from 512 tokens, Haiku 4.5 needs 4,096 — an eightfold
        // difference that decides whether caching does anything at all.
        let opus = anthropic_opus5();
        let haiku = ProviderCapabilities {
            cache: CacheSupport::Explicit {
                max_breakpoints: 4,
                ttls: vec![CacheTtl::Short, CacheTtl::Long],
                min_prefix_tokens: 4_096,
            },
            ..anthropic_opus5()
        };
        assert_eq!(opus.cache.min_prefix_tokens(), Some(512));
        assert_eq!(haiku.cache.min_prefix_tokens(), Some(4_096));
    }

    #[test]
    fn automatic_caching_offers_one_breakpoint_at_most() {
        let auto = CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: true };
        assert_eq!(auto.breakpoint_budget(), 1);
        assert!(auto.supports_ttl(CacheTtl::Short));
        assert!(!auto.supports_ttl(CacheTtl::Long), "no long-lived entries without explicit mode");

        let fixed = CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: false };
        assert_eq!(fixed.breakpoint_budget(), 0);
    }

    #[test]
    fn attempted_strict_mode_still_requires_client_validation() {
        // OpenAI's Responses API attempts strict mode and silently falls back when a schema cannot
        // be compiled, so "strict" is not a guarantee the client can rely on.
        assert!(StrictSupport::Attempted.needs_client_validation());
        assert!(StrictSupport::None.needs_client_validation());
        assert!(!StrictSupport::Always.needs_client_validation());
    }

    #[test]
    fn encrypted_reasoning_is_flagged_as_needing_a_round_trip() {
        assert!(ReasoningSupport::Encrypted.requires_round_trip());
        assert!(!ReasoningSupport::Plain.requires_round_trip());
        assert!(!ReasoningSupport::None.requires_round_trip());
    }

    #[test]
    fn prompt_budget_leaves_room_for_the_response() {
        let caps = anthropic_opus5();
        assert_eq!(caps.prompt_budget(8_000), 192_000);
        // Asking to reserve more than the model can output reserves what it can output.
        assert_eq!(caps.prompt_budget(1_000_000), 200_000 - 64_000);
    }

    #[test]
    fn capabilities_round_trip() {
        let caps = anthropic_opus5();
        let decoded: ProviderCapabilities =
            serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(decoded, caps);
    }
}
