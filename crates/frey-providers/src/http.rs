//! One HTTP client, driving any dialect.
//!
//! Retry policy, error classification, and stream decoding are written once here rather than once
//! per provider, because they are exactly the parts that get quietly wrong in a per-provider
//! rewrite. In particular: **auth and billing failures are never retried**, and a stream body is
//! never parsed eagerly (see [`crate::sse`]).

use std::sync::Arc;

use crate::streaming::async_stream;

use frey_core::ids::{ModelId, ProviderId};
use frey_core::item::Item;
use frey_core::provider::{
    EventStream, ModelProvider, ProviderError, Request, Response, StreamEvent,
};
use frey_core::provider_caps::ProviderCapabilities;
use futures_util::StreamExt;

use crate::dialect::{Auth, Dialect};
use crate::sse::{Frame, SseDecoder};

/// How many times a retryable failure is attempted before giving up.
const MAX_ATTEMPTS: u32 = 3;

/// A `ModelProvider` built from a [`Dialect`] and an HTTP endpoint.
pub struct HttpProvider {
    dialect: Arc<dyn Dialect>,
    base_url: String,
    auth: Auth,
    client: reqwest::Client,
}

impl std::fmt::Debug for HttpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProvider")
            .field("provider", &self.dialect.id())
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// How long to wait, and for what.
///
/// Two separate clocks, because "slow" and "hung" are different failures and a single total
/// deadline cannot tell them apart. A long generation is not a hang: a model may legitimately think
/// for minutes, and `z-ai/glm-4.7-flash` took 98 seconds for a three-turn run during the first live
/// session. But a connection that is accepted and then never speaks is a hang however long you
/// wait, and the default `reqwest` client waits forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Establishing the TCP and TLS connection. Short: a provider that cannot be reached quickly
    /// cannot be reached.
    pub connect_ms: u64,
    /// The gap between reads once the response has begun, *not* a deadline for the whole request.
    /// A streaming response resets this on every chunk, so a slow generation never trips it and a
    /// stalled one always does.
    pub read_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        // Ten seconds to connect, five minutes of silence to give up. The read budget is deliberate
        // rather than round: a non-streaming request to a slow reasoning model produces no bytes at
        // all until the generation finishes, so this is the real ceiling on a single completion.
        Self { connect_ms: 10_000, read_ms: 300_000 }
    }
}

impl HttpProvider {
    /// An adapter speaking `dialect` to `base_url`, with default [`Timeouts`].
    ///
    /// # Errors
    /// Returns [`ProviderError::Network`] if the HTTP client cannot be built.
    pub fn new(
        dialect: Arc<dyn Dialect>,
        base_url: impl Into<String>,
        auth: Auth,
    ) -> Result<Self, ProviderError> {
        Self::with_timeouts(dialect, base_url, auth, Timeouts::default())
    }

    /// An adapter with explicit timeouts.
    ///
    /// # Errors
    /// Returns [`ProviderError::Network`] if the HTTP client cannot be built.
    pub fn with_timeouts(
        dialect: Arc<dyn Dialect>,
        base_url: impl Into<String>,
        auth: Auth,
        timeouts: Timeouts,
    ) -> Result<Self, ProviderError> {
        let id = dialect.id();
        let client = reqwest::Client::builder()
            .user_agent(concat!("frey/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_millis(timeouts.connect_ms))
            .read_timeout(std::time::Duration::from_millis(timeouts.read_ms))
            .build()
            .map_err(|e| ProviderError::Network { provider: id, detail: e.to_string() })?;
        Ok(Self {
            dialect,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
            client,
        })
    }

    fn url(&self) -> String {
        format!("{}{}", self.base_url, self.dialect.path())
    }

    async fn send(&self, body: &serde_json::Value) -> Result<reqwest::Response, ProviderError> {
        let id = self.dialect.id();
        let mut request = self.client.post(self.url()).json(body);

        if let Some((name, value)) = self.auth.header(&id)? {
            request = request.header(name.as_str(), value);
        }
        for (name, value) in self.dialect.headers() {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request
            .send()
            .await
            .map_err(|e| ProviderError::Network { provider: id.clone(), detail: e.to_string() })?;

        let status = response.status().as_u16();
        if status >= 400 {
            let detail = response.text().await.unwrap_or_default();
            return Err(ProviderError::from_status(&id, status, truncate(&detail, 512)));
        }
        Ok(response)
    }

    /// Send with retries, honouring the error taxonomy.
    ///
    /// The rule that matters: `is_fatal` short-circuits. A 402 for exhausted credit returns fast
    /// and looks transient, and retrying it burns the rest of the run for nothing.
    async fn send_with_retry(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send(body).await {
                Ok(response) => return Ok(response),
                Err(e) if e.is_fatal() || !e.is_retryable() || attempt >= MAX_ATTEMPTS => {
                    return Err(e);
                }
                Err(e) => {
                    let backoff = backoff_ms(&e, attempt);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                }
            }
        }
    }
}

fn backoff_ms(error: &ProviderError, attempt: u32) -> u64 {
    if let ProviderError::RateLimit { retry_after_ms: Some(ms), .. } = error {
        return *ms;
    }
    // Plain exponential backoff. No jitter source here, because `frey-core` forbids randomness for
    // replay determinism and this crate keeps the same discipline; the agent layer adds jitter if
    // it wants it.
    250u64.saturating_mul(1 << attempt.min(4))
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        format!("{}… ({} more bytes)", &text[..max], text.len() - max)
    }
}

impl ModelProvider for HttpProvider {
    fn id(&self) -> ProviderId {
        self.dialect.id()
    }

    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities {
        self.dialect.capabilities(model)
    }

    async fn complete(&self, request: Request) -> Result<Response, ProviderError> {
        let body = self.dialect.encode(&request, false)?;
        let response = self.send_with_retry(&body).await?;
        let id = self.dialect.id();

        // Never `.json()` a provider response directly: read the bytes, then parse, so a
        // keepalive-prefixed or truncated body produces a protocol error naming what arrived
        // rather than an opaque decode failure.
        let text = response
            .text()
            .await
            .map_err(|e| ProviderError::Network { provider: id.clone(), detail: e.to_string() })?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| ProviderError::Protocol {
                provider: id,
                detail: format!("{e}; body began: {}", truncate(&text, 200)),
            })?;
        self.dialect.decode(&value)
    }

    async fn stream(&self, request: Request) -> Result<EventStream, ProviderError> {
        let body = self.dialect.encode(&request, true)?;
        let response = self.send_with_retry(&body).await?;
        let dialect = Arc::clone(&self.dialect);
        let id = self.dialect.id();

        let mut decoder = SseDecoder::new();
        let mut bytes = response.bytes_stream();

        let stream = async_stream(move |mut yielder| async move {
            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yielder
                            .send(Err(ProviderError::Network {
                                provider: id.clone(),
                                detail: e.to_string(),
                            }))
                            .await;
                        return;
                    }
                };
                for frame in decoder.push(&chunk) {
                    // Keepalive comments are exactly what a naive client trips over. They carry no
                    // payload and are dropped here rather than parsed.
                    let Frame::Event { data, .. } = frame else { continue };
                    if data.trim() == "[DONE]" {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
                        for event in stream_events(&value, &dialect) {
                            yielder.send(Ok(event)).await;
                        }
                    }
                }
            }
            for frame in decoder.finish() {
                if let Frame::Event { data, .. } = frame
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&data)
                {
                    for event in stream_events(&value, &dialect) {
                        yielder.send(Ok(event)).await;
                    }
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

/// Turn one streamed JSON payload into zero or more Frey stream events.
///
/// Deliberately conservative: anything unrecognised becomes a complete `Item` through the dialect's
/// decoder rather than being dropped, so a provider adding a block type degrades to "arrives whole"
/// instead of "vanishes".
fn stream_events(value: &serde_json::Value, dialect: &Arc<dyn Dialect>) -> Vec<StreamEvent> {
    let mut out = Vec::new();

    // Anthropic-shaped deltas.
    if let Some(delta) = value.get("delta") {
        if let Some(text) = delta.get("text").and_then(serde_json::Value::as_str) {
            out.push(StreamEvent::TextDelta(text.to_string()));
        }
        if let Some(text) = delta.get("thinking").and_then(serde_json::Value::as_str) {
            out.push(StreamEvent::ReasoningDelta(text.to_string()));
        }
        // OpenAI Chat-shaped deltas.
        if let Some(text) = delta.get("content").and_then(serde_json::Value::as_str) {
            out.push(StreamEvent::TextDelta(text.to_string()));
        }
    }

    // OpenAI Responses-shaped typed events.
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("response.output_text.delta") => {
            if let Some(text) = value.get("delta").and_then(serde_json::Value::as_str) {
                out.push(StreamEvent::TextDelta(text.to_string()));
            }
        }
        Some("response.completed") => {
            if let Some(response) = value.get("response")
                && let Ok(decoded) = dialect.decode(response)
            {
                out.push(StreamEvent::Done(Box::new(decoded)));
            }
        }
        _ => {}
    }

    // A terminal payload in the non-streaming shape.
    if out.is_empty()
        && (value.get("choices").is_some() || value.get("content").is_some())
        && let Ok(decoded) = dialect.decode(value)
    {
        out.push(StreamEvent::Done(Box::new(decoded)));
    }

    if out.is_empty()
        && let Ok(raw) = serde_json::value::RawValue::from_string(value.to_string())
    {
        {
            out.push(StreamEvent::Item(Box::new(Item::Opaque(frey_core::item::OpaqueItem {
                provider: dialect.id(),
                kind: value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("stream_chunk")
                    .into(),
                raw,
            }))));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::Anthropic;

    #[test]
    fn fatal_failures_short_circuit_the_retry_loop() {
        // Not a network test: the classification is what decides whether a run survives a 402.
        let id = ProviderId::new("openrouter");
        let billing = ProviderError::from_status(&id, 402, "insufficient credits");
        assert!(billing.is_fatal() && !billing.is_retryable());

        let overloaded = ProviderError::from_status(&id, 503, "");
        assert!(!overloaded.is_fatal() && overloaded.is_retryable());
    }

    #[test]
    fn backoff_honours_a_provider_supplied_delay() {
        let asked = ProviderError::RateLimit {
            provider: ProviderId::new("x"),
            retry_after_ms: Some(1_234),
        };
        assert_eq!(backoff_ms(&asked, 1), 1_234, "the provider knows better than we do");

        let unasked =
            ProviderError::RateLimit { provider: ProviderId::new("x"), retry_after_ms: None };
        assert!(backoff_ms(&unasked, 1) < backoff_ms(&unasked, 3), "and otherwise back off");
    }

    #[test]
    fn a_truncated_body_names_what_arrived() {
        let long = "x".repeat(2_000);
        let short = truncate(&long, 512);
        assert!(short.contains("more bytes"), "an operator needs to know it was cut: {short}");
        assert_eq!(truncate("short", 512), "short");
    }

    #[test]
    fn unrecognised_stream_payloads_arrive_whole_rather_than_vanishing() {
        let dialect: Arc<dyn Dialect> = Arc::new(Anthropic);
        let events = stream_events(&serde_json::json!({"type": "ping", "unknown": 1}), &dialect);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Item(_)));
    }

    #[test]
    fn text_deltas_are_recognised_in_both_provider_shapes() {
        let dialect: Arc<dyn Dialect> = Arc::new(Anthropic);
        let anthropic_shaped =
            stream_events(&serde_json::json!({"delta": {"text": "hi"}}), &dialect);
        assert_eq!(anthropic_shaped[0], StreamEvent::TextDelta("hi".into()));

        let openai_shaped = stream_events(
            &serde_json::json!({"type": "response.output_text.delta", "delta": "hi"}),
            &dialect,
        );
        assert_eq!(openai_shaped[0], StreamEvent::TextDelta("hi".into()));
    }
}
