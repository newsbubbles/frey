//! Provider adapters for [Frey](https://github.com/newsbubbles/frey).
//!
//! Each adapter is split in two, and the split is the point:
//!
//! * a [`Dialect`](dialect::Dialect) — pure functions mapping between Frey's item model and one
//!   provider's JSON, with no I/O. This is where every piece of provider nuance lives, and it is
//!   testable without a network, a key, or a mock server.
//! * an [`HttpProvider`] — one generic HTTP client that drives any dialect, so retry behaviour,
//!   error classification, and stream decoding are written once and shared.
//!
//! ```
//! use frey_providers::{anthropic::Anthropic, dialect::Dialect};
//! use frey_core::ids::ModelId;
//!
//! // Capabilities are per model, not per provider: the minimum cacheable prefix varies eightfold
//! // between models from the same vendor.
//! let opus = Anthropic.capabilities(&ModelId::new("claude-opus-5"));
//! let haiku = Anthropic.capabilities(&ModelId::new("claude-haiku-4-5-20251001"));
//! assert_eq!(opus.cache.min_prefix_tokens(), Some(512));
//! assert_eq!(haiku.cache.min_prefix_tokens(), Some(4_096));
//! ```

#[cfg(feature = "agent-cli")]
pub mod agent_cli;
pub mod anthropic;
pub mod dialect;
pub mod openai;
pub mod openrouter;
pub mod sse;
pub(crate) mod streaming;

#[cfg(feature = "http")]
mod http;
#[cfg(feature = "http")]
pub use http::{HttpProvider, Timeouts};

/// Build a dialect from a configuration entry, so a provider can be added without writing Rust.
///
/// # Errors
/// Returns the reason the configuration could not be turned into a working adapter.
pub fn dialect_from_config(
    config: &dialect::ProviderConfig,
) -> Result<Box<dyn dialect::Dialect>, String> {
    use dialect::DialectKind;
    use frey_core::ids::ProviderId;

    Ok(match config.dialect {
        DialectKind::AnthropicMessages => Box::new(anthropic::Anthropic),
        DialectKind::OpenAiResponses => Box::new(openai::OpenAiResponses),
        DialectKind::OpenAiChat => Box::new(openrouter::OpenAiChat {
            id: ProviderId::new(config.name.clone()),
            capabilities: Some(config.effective_capabilities()),
        }),
    })
}

/// The types most callers want.
pub mod prelude {
    pub use crate::anthropic::Anthropic;
    pub use crate::dialect::{Auth, Dialect, DialectKind, ProviderConfig};
    pub use crate::openai::OpenAiResponses;
    pub use crate::openrouter::{OpenAiChat, OpenRouter};
    #[cfg(feature = "http")]
    pub use crate::{HttpProvider, Timeouts};
}

#[cfg(test)]
mod tests {
    use super::*;
    use dialect::{Auth, DialectKind, ProviderConfig};
    use frey_core::ids::ModelId;

    #[test]
    fn a_provider_can_be_added_from_configuration_alone() {
        // R3: extensible by configuration, not only by code.
        let config = ProviderConfig {
            name: "internal-vllm".into(),
            dialect: DialectKind::OpenAiChat,
            base_url: "https://llm.internal/v1".into(),
            auth: Auth::Bearer { env: "VLLM_KEY".into() },
            capabilities: None,
        };
        let d = dialect_from_config(&config).unwrap();
        assert_eq!(d.id().as_str(), "internal-vllm");
        assert_eq!(d.path(), "/chat/completions");
        // And it claims nothing it was not given.
        assert!(!d.capabilities(&ModelId::new("any")).reports_cost);
    }

    #[test]
    fn every_dialect_declares_its_endpoint() {
        for kind in
            [DialectKind::AnthropicMessages, DialectKind::OpenAiResponses, DialectKind::OpenAiChat]
        {
            let config = ProviderConfig {
                name: "x".into(),
                dialect: kind,
                base_url: "https://x.test".into(),
                auth: Auth::None,
                capabilities: None,
            };
            assert!(dialect_from_config(&config).unwrap().path().starts_with('/'));
        }
    }
}
