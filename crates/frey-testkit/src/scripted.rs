//! A model that says what you told it to, and remembers what you showed it.
//!
//! The second half is the point. Most fake LLMs let you assert on what the agent *did*; the
//! interesting bugs in an agent framework are in what the model was *shown* — which tools were
//! visible, in what order, where the cache breakpoints landed, and whether reasoning state was
//! replayed. [`ScriptedModel::saw`] exposes exactly that.

use std::sync::{Arc, Mutex};

use frey_core::ids::{ModelId, ProviderId, ToolName};
use frey_core::item::Item;
use frey_core::provider::{
    EventStream, ModelProvider, ProviderError, Request, Response, StopReason, StreamEvent,
};
use frey_core::provider_caps::ProviderCapabilities;
use frey_core::segment::{CacheMark, CacheTtl};
use frey_core::usage::Usage;

/// What the scripted model should do when asked.
#[derive(Debug, Clone)]
pub enum Turn {
    /// Reply with these items.
    Reply {
        /// The items to produce.
        items: Vec<Item>,
        /// Why it stopped.
        stop: StopReason,
        /// What to report as consumed.
        usage: Usage,
    },
    /// Fail.
    Fail(ProviderError),
}

impl Turn {
    /// A plain text reply that ends the turn.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Reply {
            items: vec![Item::text(text)],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    /// A reply that asks for tools to be run.
    #[must_use]
    pub fn tool_calls(items: Vec<Item>) -> Self {
        Self::Reply { items, stop: StopReason::ToolUse, usage: Usage::default() }
    }

    /// Attach usage to a reply. Ignored for failures.
    #[must_use]
    pub fn with_usage(mut self, u: Usage) -> Self {
        if let Self::Reply { usage, .. } = &mut self {
            *usage = u;
        }
        self
    }
}

/// A `ModelProvider` that returns scripted turns and records every request.
///
/// Cheap to clone: clones share one script and one recording, so a copy handed to an agent still
/// reports back to the test that made it.
#[derive(Debug, Clone)]
pub struct ScriptedModel {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    id: ProviderId,
    caps: Mutex<ProviderCapabilities>,
    script: Mutex<Vec<Turn>>,
    next: Mutex<usize>,
    seen: Mutex<Vec<Request>>,
}

impl ScriptedModel {
    /// A model that will produce `turns`, in order.
    #[must_use]
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            inner: Arc::new(Inner {
                id: ProviderId::new("scripted"),
                caps: Mutex::new(ProviderCapabilities::minimal(200_000, 8_192)),
                script: Mutex::new(turns),
                next: Mutex::new(0),
                seen: Mutex::new(Vec::new()),
            }),
        }
    }

    /// A model that replies once with plain text.
    pub fn replying(text: impl Into<String>) -> Self {
        Self::new(vec![Turn::text(text)])
    }

    /// Present different capabilities, so a test can exercise the degradation paths.
    #[must_use]
    pub fn with_capabilities(self, caps: ProviderCapabilities) -> Self {
        *self.inner.caps.lock().expect("scripted model poisoned") = caps;
        self
    }

    /// Every request the model was given, in order.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    #[must_use]
    pub fn saw(&self) -> Vec<Request> {
        self.inner.seen.lock().expect("scripted model poisoned").clone()
    }

    /// The most recent request.
    ///
    /// # Panics
    /// If the model has not been called yet.
    #[must_use]
    pub fn last(&self) -> Request {
        self.saw().pop().expect("the model has not been called yet")
    }

    /// How many times the model was called.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.inner.seen.lock().expect("scripted model poisoned").len()
    }

    /// Whether every scripted turn was used. A test that scripts three turns and consumes one is
    /// usually asserting less than its author believed.
    #[must_use]
    pub fn script_exhausted(&self) -> bool {
        let next = *self.inner.next.lock().expect("scripted model poisoned");
        let len = self.inner.script.lock().expect("scripted model poisoned").len();
        next >= len
    }

    fn record_and_take(&self, request: Request) -> Result<Response, ProviderError> {
        let model = request.model.clone();
        self.inner.seen.lock().expect("scripted model poisoned").push(request);

        let mut next = self.inner.next.lock().expect("scripted model poisoned");
        let script = self.inner.script.lock().expect("scripted model poisoned");
        let turn = script.get(*next).cloned().unwrap_or_else(|| {
            panic!(
                "the scripted model ran out of turns: it was called {} time(s) but only {} were \
                 scripted. Either the agent looped further than expected, or the script is short.",
                *next + 1,
                script.len()
            )
        });
        *next += 1;

        match turn {
            Turn::Reply { items, stop, usage } => {
                Ok(Response { items, usage, stop, model, provider: self.inner.id.clone() })
            }
            Turn::Fail(e) => Err(e),
        }
    }
}

impl ModelProvider for ScriptedModel {
    fn id(&self) -> ProviderId {
        self.inner.id.clone()
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        self.inner.caps.lock().expect("scripted model poisoned").clone()
    }

    async fn complete(&self, request: Request) -> Result<Response, ProviderError> {
        self.record_and_take(request)
    }

    async fn stream(&self, request: Request) -> Result<EventStream, ProviderError> {
        let response = self.record_and_take(request)?;
        let mut events: Vec<Result<StreamEvent, ProviderError>> = response
            .items
            .iter()
            .map(|item| match item {
                Item::Text(t) => Ok(StreamEvent::TextDelta(t.text.clone())),
                other => Ok(StreamEvent::Item(Box::new(other.clone()))),
            })
            .collect();
        events.push(Ok(StreamEvent::Done(Box::new(response))));
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

/// What the model was shown on one request. The assertion surface.
pub trait RequestAssertions {
    /// The names of the tools that were visible, in the order they were presented.
    fn tool_names(&self) -> Vec<ToolName>;

    /// Where the cache breakpoints landed, as segment indices.
    fn breakpoints(&self) -> Vec<u32>;

    /// Whether any cache mark asks for a long-lived entry.
    fn has_long_lived_cache(&self) -> bool;

    /// Whether provider state that must be replayed verbatim actually was.
    ///
    /// Dropping a reasoning item is the classic silent regression: the model gets worse and the
    /// bill goes up, and nothing fails.
    fn replayed_provider_carry(&self) -> bool;
}

impl RequestAssertions for Request {
    fn tool_names(&self) -> Vec<ToolName> {
        self.tools.iter().map(|t| t.name.clone()).collect()
    }

    fn breakpoints(&self) -> Vec<u32> {
        self.marks.iter().map(|m: &CacheMark| m.at.index()).collect()
    }

    fn has_long_lived_cache(&self) -> bool {
        self.marks.iter().any(|m| m.ttl == CacheTtl::Long)
    }

    fn replayed_provider_carry(&self) -> bool {
        self.turns.iter().any(|t| t.has_provider_carry())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::ids::{CallId, ProviderId};
    use frey_core::item::{
        Caller, ProviderCarry, ReasoningItem, ReasoningVisibility, Role, ToolCallItem,
        Turn as CTurn,
    };
    use frey_core::provider::Effort;
    use frey_core::segment::CacheTtl;
    use frey_core::tool_def::{JsonSchema, ToolDefinition};

    fn request_with_tools(names: &[&str]) -> Request {
        Request {
            model: ModelId::new("test-model"),
            tools: names
                .iter()
                .map(|n| {
                    ToolDefinition::new(
                        *n,
                        "a tool that exists for the purposes of this test",
                        JsonSchema::empty_object(),
                    )
                })
                .collect(),
            effort: Effort::Default,
            ..Request::default()
        }
    }

    #[test]
    fn the_model_reports_what_it_was_shown() {
        let model = ScriptedModel::replying("done");
        let _ = pollster::block_on(model.complete(request_with_tools(&["fs_read", "shell"])));

        assert_eq!(model.call_count(), 1);
        assert_eq!(
            model.last().tool_names(),
            vec![ToolName::new("fs_read"), ToolName::new("shell")],
            "presentation order matters: it is the cache prefix"
        );
    }

    #[test]
    fn clones_share_one_recording() {
        // Agents take a provider by value; a test still needs to see what happened.
        let model = ScriptedModel::new(vec![Turn::text("a"), Turn::text("b")]);
        let handed_to_agent = model.clone();
        let _ = pollster::block_on(handed_to_agent.complete(Request::default()));
        assert_eq!(model.call_count(), 1, "the original observes the clone's calls");
    }

    #[test]
    fn an_unused_script_is_visible_to_the_test() {
        let model = ScriptedModel::new(vec![Turn::text("a"), Turn::text("b")]);
        let _ = pollster::block_on(model.complete(Request::default()));
        assert!(
            !model.script_exhausted(),
            "a test that scripts two turns and runs one is asserting less than it looks"
        );
    }

    #[test]
    fn cache_breakpoints_are_assertable() {
        use frey_core::ids::SegmentId;
        let model = ScriptedModel::replying("ok");
        let request = Request {
            marks: vec![
                CacheMark { at: SegmentId(1), ttl: CacheTtl::Long },
                CacheMark { at: SegmentId(4), ttl: CacheTtl::Short },
            ],
            ..Request::default()
        };
        let _ = pollster::block_on(model.complete(request));

        let seen = model.last();
        assert_eq!(seen.breakpoints(), vec![1, 4]);
        assert!(seen.has_long_lived_cache());
    }

    #[test]
    fn dropped_reasoning_state_is_detectable() {
        let model = ScriptedModel::new(vec![Turn::text("ok"), Turn::text("ok")]);

        let with_carry = CTurn::new(
            Role::Assistant,
            [Item::Reasoning(ReasoningItem {
                summary: None,
                visibility: ReasoningVisibility::Encrypted,
                carry: Some(ProviderCarry {
                    provider: ProviderId::new("openai"),
                    payload: serde_json::value::RawValue::from_string("\"blob\"".into()).unwrap(),
                }),
            })],
        );
        let _ = pollster::block_on(
            model.complete(Request { turns: vec![with_carry], ..Request::default() }),
        );
        assert!(model.last().replayed_provider_carry());

        let without = CTurn::user("hello");
        let _ = pollster::block_on(
            model.complete(Request { turns: vec![without], ..Request::default() }),
        );
        assert!(
            !model.last().replayed_provider_carry(),
            "this is the assertion that catches a lost chain of thought"
        );
    }

    #[test]
    fn failures_can_be_scripted() {
        let model = ScriptedModel::new(vec![Turn::Fail(ProviderError::Billing {
            provider: ProviderId::new("scripted"),
            detail: "no credit".into(),
        })]);
        let err = pollster::block_on(model.complete(Request::default())).unwrap_err();
        assert!(err.is_fatal(), "so retry behaviour can be tested without spending money");
    }

    #[test]
    fn streaming_yields_deltas_then_a_final_response() {
        use futures_util::StreamExt;
        let model = ScriptedModel::new(vec![Turn::tool_calls(vec![
            Item::text("thinking"),
            Item::ToolCall(ToolCallItem {
                id: CallId::new("c1"),
                name: ToolName::new("fs_read"),
                args: serde_json::json!({}),
                caller: Caller::Direct,
            }),
        ])]);

        let events: Vec<_> = pollster::block_on(async {
            let stream = model.stream(Request::default()).await.unwrap();
            stream.collect::<Vec<_>>().await
        });

        assert!(matches!(events[0], Ok(StreamEvent::TextDelta(_))));
        assert!(matches!(events[1], Ok(StreamEvent::Item(_))));
        assert!(matches!(events[2], Ok(StreamEvent::Done(_))), "the stream always ends with Done");
    }
}
