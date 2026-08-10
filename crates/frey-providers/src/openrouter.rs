//! OpenRouter, and the Chat Completions shape generally.
//!
//! Three things make this adapter different from a plain Chat Completions client:
//!
//! * **Usage accounting is always on.** OpenRouter is the one provider that reports what a call
//!   actually cost, so its ledger entries are authoritative rather than estimated.
//! * **402 means out of credit**, it returns quickly, and it looks transient. Left unclassified, a
//!   retry loop grinds through an entire run turning every turn into a silent no-op.
//! * **The upstream can change under you.** A routed request may be served by a different provider
//!   than the last one, which changes the tokenizer, the price, and whether the cache still exists.
//!   The response says who served it, and the caller is told when that changes.

use frey_core::ids::{CallId, ModelId, ProviderId, ToolName};
use frey_core::item::{
    Caller, Item, ReasoningItem, ReasoningVisibility, Role, TextItem, ToolCallItem,
};
use frey_core::provider::{ProviderError, Request, Response, StopReason};
use frey_core::provider_caps::{
    CacheSupport, Modality, ProviderCapabilities, ReasoningSupport, StrictSupport,
    ToolSearchSupport,
};
use frey_core::taint::Provenance;
use frey_core::usage::{Currency, Money, Usage};
use serde_json::{Value, json};

use crate::dialect::Dialect;

/// OpenRouter's OpenAI-compatible Chat Completions endpoint.
#[derive(Debug, Clone, Default)]
pub struct OpenRouter;

/// A plain OpenAI-compatible Chat Completions server: vLLM, Ollama, LM Studio, and most of the
/// long tail. Same wire shape, no usage accounting, no routing.
///
/// There is no `Default`: an endpoint with an empty provider id would produce ledger entries and
/// audit records that name nothing, and the compiler is a better place to catch that than a log.
#[derive(Debug, Clone)]
pub struct OpenAiChat {
    /// What this endpoint is called, since many services share the shape.
    pub id: ProviderId,
    /// What it can do. Defaults to the pessimistic baseline.
    pub capabilities: Option<ProviderCapabilities>,
}

impl OpenAiChat {
    /// An endpoint named `id`, claiming no capabilities beyond the pessimistic baseline.
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self { id: id.into(), capabilities: None }
    }
}

impl Dialect for OpenRouter {
    fn id(&self) -> ProviderId {
        ProviderId::new("openrouter")
    }

    fn path(&self) -> &str {
        "/chat/completions"
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_search: ToolSearchSupport::None,
            programmatic_tool_calling: false,
            // Whether caching is automatic or needs explicit blocks depends on the upstream the
            // router picks, so the safe assumption is automatic and the planner is told no
            // breakpoints are available.
            cache: CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: false },
            reasoning: ReasoningSupport::Plain,
            strict_schema: StrictSupport::None,
            parallel_tool_calls: true,
            input_modalities: vec![Modality::Text, Modality::Image],
            output_modalities: vec![Modality::Text],
            max_context: 128_000,
            max_output: 8_192,
            reports_cost: true,
        }
    }

    fn encode(&self, request: &Request, stream: bool) -> Result<Value, ProviderError> {
        let mut body = encode_chat(request, stream);
        // Cost is opt-in on the wire. Without this the response carries token counts and no `cost`
        // field at all, so `reports_cost: true` above is a promise the adapter breaks silently:
        // every ledger entry reads as "the provider did not say" and the one thing OpenRouter is
        // uniquely good for is switched off. Asked for here rather than left to `extra`, because a
        // caller cannot be expected to know the capability needs enabling.
        body["usage"] = json!({"include": true});
        if let Some(key) = &request.cache_key {
            // Sticky routing. Without it, affinity only begins after a cache hit is detected, so
            // the first few turns of every session scatter across upstreams.
            body["session_id"] = Value::String(key.to_string());
        }
        Ok(body)
    }

    fn decode(&self, body: &Value) -> Result<Response, ProviderError> {
        decode_chat(body, &self.id(), true)
    }
}

impl Dialect for OpenAiChat {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn path(&self) -> &str {
        "/chat/completions"
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        self.capabilities.clone().unwrap_or_else(|| ProviderCapabilities::minimal(32_768, 4_096))
    }

    fn encode(&self, request: &Request, stream: bool) -> Result<Value, ProviderError> {
        Ok(encode_chat(request, stream))
    }

    fn decode(&self, body: &Value) -> Result<Response, ProviderError> {
        decode_chat(body, &self.id(), false)
    }
}

fn encode_chat(request: &Request, stream: bool) -> Value {
    let mut messages = Vec::new();

    for turn in &request.turns {
        let role = match turn.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut text = String::new();
        let mut tool_calls = Vec::new();

        for item in &turn.items {
            match item {
                Item::Text(t) => text.push_str(&t.text),
                Item::ToolCall(c) => tool_calls.push(json!({
                    "id": c.id.as_str(),
                    "type": "function",
                    "function": {
                        "name": c.name.as_str(),
                        // Never `null`. Chat Completions wants a JSON *object* encoded as a
                        // string, and some upstreams validate it: Alibaba answers HTTP 400
                        // "the function.arguments parameter must be in JSON format" and the whole
                        // run dies. A call with no arguments has no arguments; it does not have
                        // null ones.
                        "arguments": if c.args.is_null() {
                            "{}".to_string()
                        } else {
                            c.args.to_string()
                        },
                    },
                })),
                Item::ToolResult(r) => messages.push(json!({
                    "role": "tool",
                    "tool_call_id": r.id.as_str(),
                    "content": r.content,
                })),
                // Chat Completions has nowhere to put reasoning, media, or provider-specific
                // blocks. Dropping them here is lossless overall, because the item model still
                // holds them for any provider that can.
                _ => {}
            }
        }

        if !text.is_empty() || !tool_calls.is_empty() {
            let mut message = json!({"role": role, "content": text});
            if !tool_calls.is_empty() {
                message["tool_calls"] = Value::Array(tool_calls);
            }
            messages.push(message);
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|d| {
            // Externally tagged, unlike Responses: the function lives in a nested object.
            json!({
                "type": "function",
                "function": {
                    "name": d.name.as_str(),
                    "description": d.description,
                    "parameters": d.input_schema.as_value(),
                },
            })
        })
        .collect();

    let mut body = json!({
        "model": request.model.as_str(),
        "messages": messages,
        "max_tokens": request.max_output.max(1),
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if stream {
        body["stream"] = Value::Bool(true);
    }
    for (key, value) in &request.extra {
        body[key.as_str()] = value.clone();
    }
    body
}

fn decode_chat(
    body: &Value,
    provider: &ProviderId,
    reports_cost: bool,
) -> Result<Response, ProviderError> {
    let choice =
        body.get("choices").and_then(Value::as_array).and_then(|c| c.first()).ok_or_else(|| {
            // OpenRouter answers 200 with an error object in the body when an upstream provider
            // fails, moderates, or the route dead-ends. Reporting only "no choices" throws away the
            // one sentence that says which — and that sentence is usually the actionable part.
            // Observed live: an intermittent failure on `meta-llama/llama-3.1-8b-instruct` that was
            // undiagnosable from Frey's own error message.
            let detail = match body.get("error") {
                Some(error) => {
                    let code = error
                        .get("code")
                        .map_or_else(|| "none".to_string(), std::string::ToString::to_string);
                    let message =
                        error.get("message").and_then(Value::as_str).unwrap_or("no message given");
                    format!("provider returned an error instead of a completion (code {code}): {message}")
                }
                None => format!(
                    "response has no `choices` and no `error`; body began: {}",
                    elide(&body.to_string(), 300)
                ),
            };
            ProviderError::Protocol { provider: provider.clone(), detail }
        })?;

    let message = choice.get("message").unwrap_or(&Value::Null);
    let mut items = Vec::new();

    // Reasoning first, because it came first. `capabilities()` declares `ReasoningSupport::Plain`
    // and this decoder used to drop `message.reasoning` on the floor, which is the same shape of
    // bug as claiming to report cost and never asking for it.
    //
    // The consequence is worse than a missing field. A reasoning model that spends its whole output
    // budget thinking returns `content: null` with a full `reasoning` string and
    // `finish_reason: "length"` — so the turn decoded to *zero items*, the agent loop saw no tool
    // calls, and the run ended looking like a model that had nothing to say. Observed live on
    // `openai/gpt-oss-20b:free` and `gpt-oss-120b`, where it read as the model getting worse.
    if let Some(text) = message.get("reasoning").and_then(Value::as_str)
        && !text.is_empty()
    {
        items.push(Item::Reasoning(ReasoningItem {
            summary: Some(text.to_string()),
            visibility: ReasoningVisibility::Plain,
            carry: None,
        }));
    }
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        items.push(Item::Text(TextItem {
            text: text.to_string(),
            provenance: Some(Provenance::new(format!("provider:{provider}"))),
        }));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            items.push(Item::ToolCall(ToolCallItem {
                id: CallId::new(call.get("id").and_then(Value::as_str).unwrap_or_default()),
                name: ToolName::new(
                    call.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                // An empty object rather than `Value::Null` when the arguments are absent or
                // unparseable. Null loses on the way back out — it re-encodes as the string
                // "null", which is not an object, and providers that check reject the next
                // request outright. It is also the better input to schema validation: `{}` fails
                // with "`domain` is missing", which a model can act on, and `null` does not.
                args: call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_else(|| json!({})),
                caller: Caller::Direct,
            }));
        }
    }

    // The router may substitute the upstream. Reporting the model it actually used is what lets a
    // caller notice that the price and tokenizer just changed.
    let model = ModelId::new(body.get("model").and_then(Value::as_str).unwrap_or_default());

    Ok(Response {
        items,
        usage: decode_chat_usage(body.get("usage"), reports_cost),
        stop: decode_finish(choice.get("finish_reason").and_then(Value::as_str)),
        model,
        provider: provider.clone(),
    })
}

/// Cut `text` to `max` bytes on a character boundary, saying how much was withheld.
///
/// An error message that quietly loses its tail is the same defect as a tool result that quietly
/// loses its tail, so it is reported the same way.
fn elide(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… ({} more bytes)", &text[..cut], text.len() - cut)
}

fn decode_chat_usage(usage: Option<&Value>, reports_cost: bool) -> Usage {
    let Some(u) = usage else { return Usage::default() };
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let details = u.get("prompt_tokens_details");
    let cached = details.and_then(|d| d.get("cached_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let cache_write =
        details.and_then(|d| d.get("cache_write_tokens")).and_then(Value::as_u64).unwrap_or(0);

    let reported_cost = if reports_cost {
        // Never invent a figure: `cost` absent means the provider did not say, not zero.
        u.get("cost")
            .and_then(Value::as_f64)
            .map(|c| Money { micros: (c * 1_000_000.0).round() as i64, currency: Currency::Usd })
    } else {
        None
    };

    Usage {
        input: get("prompt_tokens").saturating_sub(cached),
        output: get("completion_tokens"),
        cache_read: cached,
        cache_write,
        reasoning: 0,
        reported_cost,
        raw: serde_json::value::RawValue::from_string(u.to_string()).ok(),
    }
}

fn decode_finish(finish: Option<&str>) -> StopReason {
    match finish {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.into()),
        None => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::item::{ToolResultItem, Turn};

    fn body() -> Value {
        json!({
            "id": "gen-123",
            "model": "moonshotai/kimi-k2",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "fs_read", "arguments": "{\"path\":\"a\"}"}
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 2000,
                "completion_tokens": 30,
                "total_tokens": 2030,
                "prompt_tokens_details": {"cached_tokens": 1500, "cache_write_tokens": 500},
                "cost": 0.0125
            }
        })
    }

    #[test]
    fn a_reported_cost_is_authoritative_and_an_absent_one_is_not_invented() {
        let usage = OpenRouter.decode(&body()).unwrap().usage;
        assert_eq!(usage.reported_cost, Some(Money::usd(0.0125)));

        let mut without = body();
        without["usage"].as_object_mut().unwrap().remove("cost");
        assert_eq!(
            OpenRouter.decode(&without).unwrap().usage.reported_cost,
            None,
            "absent means unknown, not zero"
        );
    }

    #[test]
    fn cost_is_asked_for_on_the_wire_and_not_merely_decoded() {
        // The decode half of cost accounting was tested and the encode half was not, so the adapter
        // read a field it never requested. OpenRouter omits `cost` entirely unless usage accounting
        // is switched on, which made `reports_cost: true` unfalsifiable in unit tests and always
        // wrong in production. Found by putting a live run through it.
        let request = Request {
            model: ModelId::new("some/model"),
            turns: vec![Turn::user("hello")],
            max_output: 64,
            ..Request::default()
        };
        let encoded = OpenRouter.encode(&request, false).unwrap();
        assert_eq!(encoded["usage"], json!({"include": true}));

        // Not on a plain Chat Completions server: it does not report cost, and the key would be an
        // unknown field to a strict endpoint.
        let chat = OpenAiChat::new(ProviderId::new("internal-vllm"));
        assert!(chat.encode(&request, false).unwrap().get("usage").is_none());
    }

    /// A reasoning model that spent its whole budget thinking is not a model with nothing to say.
    ///
    /// `openai/gpt-oss-20b:free` returns `content: null`, a full `reasoning` string, and
    /// `finish_reason: "length"`. Dropping `reasoning` decoded that to zero items, so the agent
    /// loop saw an assistant turn with no text and no tool calls and ended the run — reported as a
    /// model producing empty turns, which blocked a tier selection for an afternoon. The adapter
    /// declares `ReasoningSupport::Plain` and has to mean it.
    #[test]
    fn a_reasoning_only_answer_is_not_an_empty_one() {
        let body = json!({
            "id": "gen-1",
            "model": "openai/gpt-oss-20b:free",
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "The user wants a guest book. I should call site_write first…"
                }
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        });

        let response = OpenRouter.decode(&body).unwrap();
        assert_eq!(response.stop, StopReason::MaxTokens, "truncation is still truncation");
        assert_eq!(response.items.len(), 1, "the thinking is the turn: {:?}", response.items);
        let Item::Reasoning(reasoning) = &response.items[0] else {
            panic!("expected reasoning, got {:?}", response.items[0])
        };
        assert!(reasoning.summary.as_deref().unwrap().contains("site_write"));
    }

    /// And when there is both, the thinking comes before the answer.
    #[test]
    fn reasoning_precedes_the_text_it_produced() {
        let body = json!({
            "id": "gen-2",
            "model": "m",
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ready", "reasoning": "thinking"}
            }]
        });
        let items = OpenRouter.decode(&body).unwrap().items;
        assert!(matches!(items[0], Item::Reasoning(_)), "{items:?}");
        assert!(matches!(items[1], Item::Text(_)), "{items:?}");
    }

    /// A tool call with no arguments must not come back out as `null`.
    ///
    /// The round trip is what breaks: a model emits a call with absent or unparseable arguments,
    /// decode turned that into `Value::Null`, and the next request re-encoded it as the *string*
    /// `"null"`. Chat Completions wants a JSON object there, and upstreams that check say so —
    /// Alibaba answers HTTP 400 "the function.arguments parameter of the code model must be in
    /// JSON format" and every remaining turn of the run fails the same way. Found on
    /// `qwen/qwen3.7-flash`, where it disqualified the model for something frey did.
    #[test]
    fn a_tool_call_without_arguments_round_trips_as_an_object() {
        let body = json!({
            "id": "gen-1",
            "model": "m",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "money_balance", "arguments": ""}
                    }]
                }
            }]
        });

        let items = OpenRouter.decode(&body).unwrap().items;
        let Item::ToolCall(call) = &items[0] else { panic!("{items:?}") };
        assert_eq!(call.args, json!({}), "absent arguments are an empty object, not null");

        // And back out again, which is where it used to become the string "null".
        let request = Request {
            model: ModelId::new("m"),
            turns: vec![Turn::new(Role::Assistant, items.clone())],
            max_output: 64,
            ..Request::default()
        };
        let encoded = OpenRouter.encode(&request, false).unwrap();
        let arguments = encoded["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments are a string on the wire");
        assert_eq!(arguments, "{}");
        assert!(
            serde_json::from_str::<Value>(arguments).unwrap().is_object(),
            "whatever we send must parse as an object"
        );
    }

    #[test]
    fn a_plain_chat_endpoint_never_claims_to_report_cost() {
        // The same body through a vLLM-shaped endpoint: a `cost` field there would be someone
        // else's convention, not a figure Frey may bill against.
        let chat = OpenAiChat::new(ProviderId::new("internal-vllm"));
        assert_eq!(chat.decode(&body()).unwrap().usage.reported_cost, None);
        assert!(!chat.capabilities(&ModelId::new("any")).reports_cost);
    }

    #[test]
    fn the_upstream_that_actually_served_the_request_is_reported() {
        // A router substituting a model changes price, tokenizer, and cache validity. A caller that
        // cannot see the substitution cannot account for any of it.
        let response = OpenRouter.decode(&body()).unwrap();
        assert_eq!(response.model, ModelId::new("moonshotai/kimi-k2"));
    }

    #[test]
    fn cached_tokens_are_subtracted_from_the_prompt_total() {
        let usage = OpenRouter.decode(&body()).unwrap().usage;
        assert_eq!(usage.input, 500);
        assert_eq!(usage.cache_read, 1_500);
        assert_eq!(usage.cache_write, 500);
        assert_eq!(usage.total_input(), 2_500);
    }

    #[test]
    fn tool_calls_survive_the_string_encoded_arguments_convention() {
        let response = OpenRouter.decode(&body()).unwrap();
        let Item::ToolCall(call) = &response.items[0] else { panic!("expected a tool call") };
        assert_eq!(call.name, ToolName::new("fs_read"));
        assert_eq!(call.args, json!({"path": "a"}));
        assert!(response.stop.wants_tools());
    }

    #[test]
    fn chat_completions_nests_the_function_unlike_responses() {
        use frey_core::tool_def::{JsonSchema, ToolDefinition};
        let request = Request {
            tools: vec![ToolDefinition::new(
                "fs_read",
                "Read a file from the workspace and return its text",
                JsonSchema::empty_object(),
            )],
            max_output: 100,
            ..Request::default()
        };
        let encoded = OpenRouter.encode(&request, false).unwrap();
        assert_eq!(encoded["tools"][0]["function"]["name"], json!("fs_read"));
    }

    #[test]
    fn a_session_id_is_sent_for_sticky_routing() {
        let request =
            Request { cache_key: Some("run-7".into()), max_output: 100, ..Request::default() };
        assert_eq!(OpenRouter.encode(&request, false).unwrap()["session_id"], json!("run-7"));
    }

    #[test]
    fn tool_results_become_their_own_message() {
        use frey_core::item::Turn;
        let request = Request {
            turns: vec![Turn::new(
                Role::User,
                [Item::ToolResult(ToolResultItem {
                    id: CallId::new("call_1"),
                    content: "file contents".into(),
                    is_error: false,
                    bytes_elided: 0,
                    provenance: Provenance::new("tool:fs_read"),
                })],
            )],
            max_output: 100,
            ..Request::default()
        };
        let encoded = OpenRouter.encode(&request, false).unwrap();
        assert_eq!(encoded["messages"][0]["role"], json!("tool"));
        assert_eq!(encoded["messages"][0]["tool_call_id"], json!("call_1"));
    }
}
