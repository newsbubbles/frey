//! Wire dialects: the pure half of a provider adapter.
//!
//! A [`Dialect`] maps between Frey's item model and one provider's JSON, with **no I/O**. That
//! split is what makes the whole wire mapping testable without a network, a key, or a mock server:
//! `encode` and `decode` are functions, and the round-trip conformance test that keeps
//! normalisation honest is an ordinary unit test.
//!
//! It is also how R3 is met. A provider that speaks a shape Frey already knows needs only a
//! `frey.toml` entry naming the dialect, a base URL, and an auth scheme — no Rust.

use frey_core::ids::{ModelId, ProviderId};
use frey_core::provider::{ProviderError, Request, Response};
use frey_core::provider_caps::ProviderCapabilities;
use smol_str::SmolStr;

/// How a provider authenticates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Auth {
    /// `Authorization: Bearer <value>`.
    Bearer {
        /// Environment variable holding the token. The value is never stored in configuration.
        env: SmolStr,
    },
    /// A custom header, e.g. Anthropic's `x-api-key`.
    Header {
        /// Header name.
        name: SmolStr,
        /// Environment variable holding the value.
        env: SmolStr,
    },
    /// No authentication, for local servers.
    None,
}

impl Auth {
    /// The header this scheme contributes, reading the secret from the environment.
    ///
    /// # Errors
    /// Returns [`ProviderError::Auth`] when the named variable is unset, naming the variable — an
    /// unset key should say which key, not "unauthorized".
    pub fn header(
        &self,
        provider: &ProviderId,
    ) -> Result<Option<(SmolStr, String)>, ProviderError> {
        let (name, env) = match self {
            Self::Bearer { env } => (SmolStr::new("authorization"), env),
            Self::Header { name, env } => (name.clone(), env),
            Self::None => return Ok(None),
        };
        let value = std::env::var(env.as_str()).map_err(|_| ProviderError::Auth {
            provider: provider.clone(),
            detail: format!("environment variable `{env}` is not set"),
        })?;
        let value =
            if matches!(self, Self::Bearer { .. }) { format!("Bearer {value}") } else { value };
        Ok(Some((name, value)))
    }
}

/// The pure wire mapping for one provider shape.
pub trait Dialect: Send + Sync + 'static {
    /// Which adapter this is.
    fn id(&self) -> ProviderId;

    /// The path appended to the base URL.
    fn path(&self) -> &str;

    /// Extra headers this provider requires, beyond authentication.
    fn headers(&self) -> Vec<(SmolStr, SmolStr)> {
        Vec::new()
    }

    /// What this provider can do for a model.
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;

    /// Frey request to provider JSON.
    ///
    /// # Errors
    /// Returns [`ProviderError::Unsupported`] rather than silently dropping something the provider
    /// cannot express.
    fn encode(&self, request: &Request, stream: bool) -> Result<serde_json::Value, ProviderError>;

    /// Provider JSON to a Frey response.
    ///
    /// # Errors
    /// Returns [`ProviderError::Protocol`] when the body does not match the documented shape.
    fn decode(&self, body: &serde_json::Value) -> Result<Response, ProviderError>;
}

/// Which known wire shape a configured provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DialectKind {
    /// OpenAI's Responses API: typed items, `instructions`, `input`.
    OpenAiResponses,
    /// OpenAI Chat Completions, and the many services that copy it.
    OpenAiChat,
    /// Anthropic's Messages API.
    AnthropicMessages,
}

/// A provider defined entirely in configuration.
///
/// The capability overrides matter more than they look: a config-defined provider starts from
/// [`ProviderCapabilities::minimal`], so a feature nobody declared is **absent** rather than
/// assumed. Claiming a capability you do not have is worse than not having it, because the
/// framework will then plan around a lie.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    /// The name this provider is referred to by.
    pub name: SmolStr,
    /// Which wire shape it speaks.
    pub dialect: DialectKind,
    /// Base URL, without a trailing slash.
    pub base_url: String,
    /// How to authenticate.
    pub auth: Auth,
    /// What it can do. Defaults to the pessimistic baseline.
    #[serde(default)]
    pub capabilities: Option<ProviderCapabilities>,
}

impl ProviderConfig {
    /// The capabilities to use, falling back to the pessimistic baseline.
    #[must_use]
    pub fn effective_capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone().unwrap_or_else(|| ProviderCapabilities::minimal(32_768, 4_096))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_key_names_the_variable_rather_than_saying_unauthorized() {
        let auth = Auth::Bearer { env: "FREY_TEST_DEFINITELY_UNSET".into() };
        let err = auth.header(&ProviderId::new("test")).unwrap_err();
        assert!(err.is_fatal(), "a missing key is not something to retry");
        assert!(
            format!("{err}").contains("FREY_TEST_DEFINITELY_UNSET"),
            "the operator must learn which variable: {err}"
        );
    }

    #[test]
    fn no_auth_contributes_no_header() {
        assert_eq!(Auth::None.header(&ProviderId::new("local")).unwrap(), None);
    }

    #[test]
    fn a_config_defined_provider_claims_nothing_it_was_not_given() {
        let config = ProviderConfig {
            name: "internal-vllm".into(),
            dialect: DialectKind::OpenAiChat,
            base_url: "https://llm.internal/v1".into(),
            auth: Auth::Bearer { env: "VLLM_KEY".into() },
            capabilities: None,
        };
        let caps = config.effective_capabilities();
        assert!(!caps.tool_search.is_native());
        assert!(!caps.programmatic_tool_calling);
        assert_eq!(caps.cache.breakpoint_budget(), 0);
        assert!(!caps.reports_cost);
    }

    #[test]
    fn config_round_trips_so_frey_toml_and_the_builder_cannot_drift() {
        let config = ProviderConfig {
            name: "internal".into(),
            dialect: DialectKind::AnthropicMessages,
            base_url: "https://x.test".into(),
            auth: Auth::Header { name: "x-api-key".into(), env: "K".into() },
            capabilities: None,
        };
        let decoded: ProviderConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(decoded, config);
    }
}
