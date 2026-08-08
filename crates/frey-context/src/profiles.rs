//! Capability profiles for real providers.
//!
//! These are the numbers from the vendors' own documentation, gathered in
//! `notes/research/02-provider-nuance-matrix.md`. They live here rather than in each adapter so the
//! planner can be tested against every shape of provider without any of them existing yet, and so
//! that a change in, say, a minimum cacheable prefix is a one-line diff with a test that notices.
//!
//! Each profile carries the date it was checked. They are documented facts about someone else's
//! service, so they go stale; `frey doctor` compares them against what a provider actually reports.

use frey_core::provider_caps::{
    CacheSupport, Modality, ProviderCapabilities, ReasoningSupport, StrictSupport,
    ToolSearchSupport,
};
use frey_core::segment::CacheTtl;

/// When these figures were last checked against vendor documentation.
pub const CHECKED: &str = "2026-08-08";

fn anthropic_base(
    min_prefix_tokens: u32,
    max_context: u32,
    max_output: u32,
) -> ProviderCapabilities {
    ProviderCapabilities {
        // Regex and BM25 variants, five results per search, ten thousand deferred tools per request.
        tool_search: ToolSearchSupport::Native { max_results: 5, max_deferred: 10_000 },
        programmatic_tool_calling: true,
        cache: CacheSupport::Explicit {
            // Four explicit breakpoints; automatic caching consumes one of them.
            max_breakpoints: 4,
            ttls: vec![CacheTtl::Short, CacheTtl::Long],
            min_prefix_tokens,
        },
        reasoning: ReasoningSupport::Plain,
        strict_schema: StrictSupport::Always,
        parallel_tool_calls: true,
        input_modalities: vec![Modality::Text, Modality::Image, Modality::Document],
        output_modalities: vec![Modality::Text],
        max_context,
        max_output,
        // Anthropic report tokens, not money. Any figure Frey shows is an estimate.
        reports_cost: false,
    }
}

/// Claude Opus 5. Minimum cacheable prefix: 512 tokens.
#[must_use]
pub fn opus5() -> ProviderCapabilities {
    anthropic_base(512, 200_000, 64_000)
}

/// Claude Haiku 4.5. Minimum cacheable prefix: 4,096 tokens — eight times Opus 5's, which is why
/// the planner takes capabilities per model rather than per provider.
#[must_use]
pub fn haiku45() -> ProviderCapabilities {
    anthropic_base(4_096, 200_000, 8_192)
}

/// Claude Sonnet 5. Minimum cacheable prefix: 1,024 tokens.
#[must_use]
pub fn sonnet5() -> ProviderCapabilities {
    anthropic_base(1_024, 200_000, 64_000)
}

/// An OpenAI Responses-API model: caching is automatic above 1,024 tokens, with an explicit mode
/// available, and reasoning comes back encrypted and must be replayed verbatim.
#[must_use]
pub fn openai() -> ProviderCapabilities {
    ProviderCapabilities {
        tool_search: ToolSearchSupport::Native { max_results: 5, max_deferred: 10_000 },
        programmatic_tool_calling: false,
        cache: CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: true },
        reasoning: ReasoningSupport::Encrypted,
        // Responses attempts strict mode and falls back silently if a schema will not compile, so
        // the client must still validate.
        strict_schema: StrictSupport::Attempted,
        parallel_tool_calls: true,
        input_modalities: vec![Modality::Text, Modality::Image, Modality::Document],
        output_modalities: vec![Modality::Text],
        max_context: 400_000,
        max_output: 128_000,
        reports_cost: false,
    }
}

/// A model routed through OpenRouter whose upstream caches automatically.
///
/// OpenRouter is the one provider that reports cost, which is why the ledger can be complete for it
/// and only estimated elsewhere.
#[must_use]
pub fn openrouter_automatic() -> ProviderCapabilities {
    ProviderCapabilities {
        tool_search: ToolSearchSupport::None,
        programmatic_tool_calling: false,
        cache: CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: false },
        reasoning: ReasoningSupport::Plain,
        strict_schema: StrictSupport::None,
        parallel_tool_calls: true,
        input_modalities: vec![Modality::Text],
        output_modalities: vec![Modality::Text],
        max_context: 128_000,
        max_output: 8_192,
        reports_cost: true,
    }
}

/// A model routed through OpenRouter whose upstream needs explicit `cache_control` blocks.
#[must_use]
pub fn openrouter_explicit() -> ProviderCapabilities {
    ProviderCapabilities {
        cache: CacheSupport::Explicit {
            max_breakpoints: 4,
            ttls: vec![CacheTtl::Short, CacheTtl::Long],
            min_prefix_tokens: 1_024,
        },
        ..openrouter_automatic()
    }
}

/// A self-hosted or older model with no prompt caching at all.
#[must_use]
pub fn no_cache() -> ProviderCapabilities {
    ProviderCapabilities::minimal(32_768, 4_096)
}

/// Every profile, for tests that must hold across all provider shapes.
#[must_use]
pub fn all() -> Vec<(&'static str, ProviderCapabilities)> {
    vec![
        ("opus5", opus5()),
        ("sonnet5", sonnet5()),
        ("haiku45", haiku45()),
        ("openai", openai()),
        ("openrouter_automatic", openrouter_automatic()),
        ("openrouter_explicit", openrouter_explicit()),
        ("no_cache", no_cache()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_prefixes_differ_by_model_within_one_vendor() {
        assert_eq!(opus5().cache.min_prefix_tokens(), Some(512));
        assert_eq!(sonnet5().cache.min_prefix_tokens(), Some(1_024));
        assert_eq!(haiku45().cache.min_prefix_tokens(), Some(4_096));
    }

    #[test]
    fn automatic_caching_offers_fewer_breakpoints_than_explicit() {
        assert_eq!(opus5().cache.breakpoint_budget(), 4);
        assert_eq!(openai().cache.breakpoint_budget(), 1);
        assert_eq!(openrouter_automatic().cache.breakpoint_budget(), 0);
        assert_eq!(no_cache().cache.breakpoint_budget(), 0);
    }

    #[test]
    fn only_one_profile_reports_cost() {
        let reporting: Vec<&str> =
            all().iter().filter(|(_, c)| c.reports_cost).map(|(n, _)| *n).collect();
        assert_eq!(
            reporting,
            ["openrouter_automatic", "openrouter_explicit"],
            "everywhere else, a cost figure is an estimate and must be labelled as one"
        );
    }

    #[test]
    fn openai_reasoning_must_be_round_tripped() {
        assert!(openai().reasoning.requires_round_trip());
        assert!(!opus5().reasoning.requires_round_trip());
    }
}
