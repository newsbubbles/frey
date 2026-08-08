//! OpenAI's Responses API, and the Chat Completions shape that many services copy.
//!
//! Responses is the target (ADR-0003): its `output[]` is a list of typed items, which is what
//! Frey's conversation model already is. The trap it exists to avoid is **reasoning items**. With
//! `store: false`, reasoning comes back carrying `encrypted_content`, and dropping it is silent:
//! the model loses the chain of thought it already produced, answers get worse, and you pay to
//! regenerate it. Nothing errors. So [`OpenAiResponses`] round-trips `ProviderCarry` verbatim and a
//! test proves it.

use frey_core::ids::{CallId, ModelId, ProviderId, ToolName};
use frey_core::item::{
    Caller, Item, OpaqueItem, ProviderCarry, ReasoningItem, ReasoningVisibility, Role, TextItem,
    ToolCallItem, ToolResultItem,
};
use frey_core::provider::{ProviderError, Request, Response, StopReason};
use frey_core::provider_caps::{
    CacheSupport, Modality, ProviderCapabilities, ReasoningSupport, StrictSupport,
    ToolSearchSupport,
};
use frey_core::taint::Provenance;
use frey_core::usage::Usage;
use serde_json::{Value, json};

use crate::dialect::Dialect;

/// OpenAI's Responses API.
#[derive(Debug, Clone, Default)]
pub struct OpenAiResponses;

impl Dialect for OpenAiResponses {
    fn id(&self) -> ProviderId {
        ProviderId::new("openai")
    }

    fn path(&self) -> &str {
        "/responses"
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_search: ToolSearchSupport::Native { max_results: 5, max_deferred: 10_000 },
            programmatic_tool_calling: false,
            cache: CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: true },
            reasoning: ReasoningSupport::Encrypted,
            // Responses attempts strict mode and falls back silently when a schema will not
            // compile, so "strict" is not a guarantee the client may rely on.
            strict_schema: StrictSupport::Attempted,
            parallel_tool_calls: true,
            input_modalities: vec![Modality::Text, Modality::Image, Modality::Document],
            output_modalities: vec![Modality::Text],
            max_context: 400_000,
            max_output: 128_000,
            reports_cost: false,
        }
    }

    fn encode(&self, request: &Request, stream: bool) -> Result<Value, ProviderError> {
        let mut instructions = String::new();
        let mut input = Vec::new();

        for turn in &request.turns {
            if turn.role == Role::System {
                for item in &turn.items {
                    if let Item::Text(t) = item {
                        if !instructions.is_empty() {
                            instructions.push('\n');
                        }
                        instructions.push_str(&t.text);
                    }
                }
                continue;
            }
            let role = if turn.role == Role::User { "user" } else { "assistant" };
            for item in &turn.items {
                if let Some(encoded) = encode_item(item, role) {
                    input.push(encoded);
                }
            }
        }

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|d| {
                // Responses uses internal tagging with flat fields, unlike Chat Completions'
                // nested `function` object.
                json!({
                    "type": "function",
                    "name": d.name.as_str(),
                    "description": d.description,
                    "parameters": d.input_schema.as_value(),
                })
            })
            .collect();

        let mut body = json!({
            "model": request.model.as_str(),
            "input": input,
            "max_output_tokens": request.max_output.max(1),
            // Without this, reasoning items come back without `encrypted_content` and the chain of
            // thought cannot be replayed on the next turn.
            "store": false,
            "include": ["reasoning.encrypted_content"],
        });
        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(key) = &request.cache_key {
            // Routes related requests to the same cache server. The documented guidance is to keep
            // traffic per key near fifteen requests a minute; beyond that, hit rate degrades.
            body["prompt_cache_key"] = Value::String(key.to_string());
        }
        if stream {
            body["stream"] = Value::Bool(true);
        }
        for (key, value) in &request.extra {
            body[key.as_str()] = value.clone();
        }
        Ok(body)
    }

    fn decode(&self, body: &Value) -> Result<Response, ProviderError> {
        let provider = self.id();
        let output = body.get("output").and_then(Value::as_array).ok_or_else(|| {
            ProviderError::Protocol {
                provider: provider.clone(),
                detail: "response has no `output` array".into(),
            }
        })?;

        let items: Vec<Item> = output.iter().map(|o| decode_output_item(o, &provider)).collect();
        let wants_tools = items.iter().any(|i| matches!(i, Item::ToolCall(_)));
        let status = body.get("status").and_then(Value::as_str);

        Ok(Response {
            items,
            usage: decode_usage(body.get("usage")),
            stop: decode_stop(status, wants_tools),
            model: ModelId::new(body.get("model").and_then(Value::as_str).unwrap_or_default()),
            provider,
        })
    }
}

fn encode_item(item: &Item, role: &str) -> Option<Value> {
    match item {
        Item::Text(t) => Some(json!({
            "type": "message",
            "role": role,
            "content": [{"type": if role == "user" { "input_text" } else { "output_text" }, "text": t.text}],
        })),
        Item::ToolCall(c) => Some(json!({
            "type": "function_call",
            "call_id": c.id.as_str(),
            "name": c.name.as_str(),
            "arguments": c.args.to_string(),
        })),
        Item::ToolResult(r) => Some(json!({
            "type": "function_call_output",
            "call_id": r.id.as_str(),
            "output": r.content,
        })),
        // The whole point: replay the reasoning item exactly as it arrived, encrypted payload and
        // all. Dropping it is free to do and expensive to have done.
        Item::Reasoning(r) => {
            r.carry.as_ref().and_then(|c| serde_json::from_str::<Value>(c.payload.get()).ok())
        }
        Item::Opaque(o) => serde_json::from_str::<Value>(o.raw.get()).ok(),
        // `Item` is non-exhaustive; see the note in the Anthropic adapter.
        _ => None,
    }
}

fn decode_output_item(item: &Value, provider: &ProviderId) -> Item {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "message" => {
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            Item::Text(TextItem { text, provenance: Some(Provenance::new("provider:openai")) })
        }
        "reasoning" => Item::Reasoning(ReasoningItem {
            summary: item
                .get("summary")
                .and_then(Value::as_array)
                .and_then(|s| s.first())
                .and_then(|s| s.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string),
            visibility: if item.get("encrypted_content").is_some() {
                ReasoningVisibility::Encrypted
            } else {
                ReasoningVisibility::Plain
            },
            carry: raw_of(item)
                .map(|payload| ProviderCarry { provider: provider.clone(), payload }),
        }),
        "function_call" => Item::ToolCall(ToolCallItem {
            id: CallId::new(item.get("call_id").and_then(Value::as_str).unwrap_or_default()),
            name: ToolName::new(item.get("name").and_then(Value::as_str).unwrap_or_default()),
            args: item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null),
            caller: Caller::Direct,
        }),
        "function_call_output" => Item::ToolResult(ToolResultItem {
            id: CallId::new(item.get("call_id").and_then(Value::as_str).unwrap_or_default()),
            content: item.get("output").and_then(Value::as_str).unwrap_or_default().to_string(),
            is_error: false,
            bytes_elided: 0,
            provenance: Provenance::new("provider:openai"),
        }),
        other => Item::Opaque(OpaqueItem {
            provider: provider.clone(),
            kind: other.into(),
            raw: raw_of(item).unwrap_or_else(|| {
                serde_json::value::RawValue::from_string("null".into()).expect("valid JSON")
            }),
        }),
    }
}

fn raw_of(value: &Value) -> Option<Box<serde_json::value::RawValue>> {
    serde_json::value::RawValue::from_string(value.to_string()).ok()
}

fn decode_usage(usage: Option<&Value>) -> Usage {
    let Some(u) = usage else { return Usage::default() };
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let details = u.get("input_tokens_details");
    let cached = details.and_then(|d| d.get("cached_tokens")).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        // Unlike Anthropic, OpenAI's `input_tokens` is the total, and cached tokens are a subset.
        input: get("input_tokens").saturating_sub(cached),
        output: get("output_tokens"),
        cache_read: cached,
        cache_write: get("cache_write_tokens"),
        reasoning: u
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reported_cost: None,
        raw: raw_of(u),
    }
}

fn decode_stop(status: Option<&str>, wants_tools: bool) -> StopReason {
    match status {
        Some("incomplete") => StopReason::MaxTokens,
        _ if wants_tools => StopReason::ToolUse,
        Some("completed") | None => StopReason::EndTurn,
        Some(other) => StopReason::Other(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::item::Turn;

    fn body() -> Value {
        json!({
            "model": "gpt-5.6",
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": [{"text": "weighed three options"}],
                 "encrypted_content": "gAAAAABm-opaque-blob"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Here you go."}]},
                {"type": "web_search_call", "id": "ws_1", "status": "completed"}
            ],
            "usage": {
                "input_tokens": 5000,
                "output_tokens": 42,
                "input_tokens_details": {"cached_tokens": 4096},
                "output_tokens_details": {"reasoning_tokens": 30}
            }
        })
    }

    #[test]
    fn encrypted_reasoning_is_replayed_verbatim() {
        // The silent, expensive regression this whole adapter is shaped around.
        let response = OpenAiResponses.decode(&body()).unwrap();
        let reasoning = response
            .items
            .iter()
            .find_map(|i| match i {
                Item::Reasoning(r) => Some(r),
                _ => None,
            })
            .expect("reasoning item");
        assert_eq!(reasoning.visibility, ReasoningVisibility::Encrypted);
        assert!(reasoning.carry.is_some(), "the encrypted payload must be retained");

        let request = Request {
            model: ModelId::new("gpt-5.6"),
            turns: vec![Turn::new(Role::Assistant, response.items.clone())],
            max_output: 1024,
            ..Request::default()
        };
        let encoded = OpenAiResponses.encode(&request, false).unwrap();
        let replayed = &encoded["input"][0];
        assert_eq!(replayed["encrypted_content"], json!("gAAAAABm-opaque-blob"));
        assert_eq!(replayed["type"], json!("reasoning"));
    }

    #[test]
    fn the_request_asks_for_encrypted_reasoning_in_the_first_place() {
        // Without `store: false` plus the include, there is nothing to replay and the trap is
        // sprung before the response even arrives.
        let encoded = OpenAiResponses
            .encode(&Request { max_output: 100, ..Request::default() }, false)
            .unwrap();
        assert_eq!(encoded["store"], json!(false));
        assert_eq!(encoded["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn hosted_tool_calls_are_preserved_rather_than_dropped() {
        let response = OpenAiResponses.decode(&body()).unwrap();
        let opaque = response
            .items
            .iter()
            .find_map(|i| match i {
                Item::Opaque(o) => Some(o),
                _ => None,
            })
            .expect("web_search_call must survive");
        assert_eq!(opaque.kind.as_str(), "web_search_call");
    }

    #[test]
    fn cached_tokens_are_not_double_counted() {
        // OpenAI's input_tokens includes cached tokens; Anthropic's excludes them. Getting this
        // backwards inflates or deflates every cost estimate.
        let usage = OpenAiResponses.decode(&body()).unwrap().usage;
        assert_eq!(usage.input, 904);
        assert_eq!(usage.cache_read, 4_096);
        assert_eq!(usage.total_input(), 5_000, "matches what the provider reported");
        assert_eq!(usage.reasoning, 30);
    }

    #[test]
    fn tool_definitions_use_flat_internal_tagging() {
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
        let encoded = OpenAiResponses.encode(&request, false).unwrap();
        assert_eq!(encoded["tools"][0]["type"], json!("function"));
        assert_eq!(
            encoded["tools"][0]["name"],
            json!("fs_read"),
            "flat, not nested under `function`"
        );
    }

    #[test]
    fn an_incomplete_response_is_not_mistaken_for_a_finished_one() {
        let mut b = body();
        b["status"] = json!("incomplete");
        assert!(OpenAiResponses.decode(&b).unwrap().stop.is_truncated());
    }

    #[test]
    fn a_prompt_cache_key_is_forwarded_for_routing_affinity() {
        let request =
            Request { cache_key: Some("session-42".into()), max_output: 100, ..Request::default() };
        let encoded = OpenAiResponses.encode(&request, false).unwrap();
        assert_eq!(encoded["prompt_cache_key"], json!("session-42"));
    }
}
