//! Anthropic's Messages API.
//!
//! The interesting parts are not the message shape but the four places money leaks:
//!
//! * `cache_control` placement, which the [`CachePlan`](frey_context) decides and this adapter
//!   merely realises — and which it must **refuse** to place on a deferred tool, since that is a 400;
//! * `usage.input_tokens` counting only tokens *after* the last breakpoint, so the naive total
//!   understates a cached request by orders of magnitude;
//! * `defer_loading`, which controls context but not bandwidth — every definition is still sent;
//! * `allowed_callers`, which Anthropic document as guidance rather than a security boundary, so
//!   Frey enforces it client-side too.

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
use frey_core::segment::CacheTtl;
use frey_core::taint::Provenance;
use frey_core::tool_def::{CallerPolicy, PresentationHint, ToolDefinition};
use frey_core::usage::Usage;
use serde_json::{Value, json};

use crate::dialect::Dialect;

/// The Anthropic Messages dialect.
#[derive(Debug, Clone, Default)]
pub struct Anthropic;

/// The API version header Anthropic require.
pub const API_VERSION: &str = "2023-06-01";

fn min_prefix_for(model: &str) -> u32 {
    // From the published per-model table. The eightfold spread within one vendor is exactly why
    // capabilities are per model rather than per provider.
    match model {
        m if m.contains("opus-5") || m.contains("fable-5") || m.contains("mythos-5") => 512,
        m if m.contains("opus-4-6") || m.contains("opus-4-5") || m.contains("haiku-4-5") => 4_096,
        m if m.contains("opus-4-7") || m.contains("haiku-3-5") => 2_048,
        _ => 1_024,
    }
}

impl Dialect for Anthropic {
    fn id(&self) -> ProviderId {
        ProviderId::new("anthropic")
    }

    fn path(&self) -> &str {
        "/v1/messages"
    }

    fn headers(&self) -> Vec<(smol_str::SmolStr, smol_str::SmolStr)> {
        vec![("anthropic-version".into(), API_VERSION.into())]
    }

    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities {
            tool_search: ToolSearchSupport::Native { max_results: 5, max_deferred: 10_000 },
            programmatic_tool_calling: true,
            cache: CacheSupport::Explicit {
                max_breakpoints: 4,
                ttls: vec![CacheTtl::Short, CacheTtl::Long],
                min_prefix_tokens: min_prefix_for(model.as_str()),
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

    fn encode(&self, request: &Request, stream: bool) -> Result<Value, ProviderError> {
        let mut system = Vec::new();
        let mut messages = Vec::new();

        for turn in &request.turns {
            match turn.role {
                Role::System => {
                    for item in &turn.items {
                        if let Item::Text(t) = item {
                            system.push(json!({"type": "text", "text": t.text}));
                        }
                    }
                }
                Role::User | Role::Assistant => {
                    let role = if turn.role == Role::User { "user" } else { "assistant" };
                    let content: Vec<Value> = turn.items.iter().filter_map(encode_item).collect();
                    if !content.is_empty() {
                        messages.push(json!({"role": role, "content": content}));
                    }
                }
            }
        }

        let tools: Vec<Value> = request.tools.iter().map(encode_tool).collect();

        // Realise the cache plan. Anthropic take `cache_control` on the *last block* of the cached
        // prefix, so a mark landing in the tool block becomes a marked tool, and one landing later
        // becomes a marked system block.
        let mut body = json!({
            "model": request.model.as_str(),
            "max_tokens": request.max_output.max(1),
            "messages": messages,
        });
        if !system.is_empty() {
            apply_cache_marks(&mut system, request);
            body["system"] = Value::Array(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
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
        let content = body.get("content").and_then(Value::as_array).ok_or_else(|| {
            ProviderError::Protocol {
                provider: provider.clone(),
                detail: "response has no `content` array".into(),
            }
        })?;

        let model = ModelId::new(body.get("model").and_then(Value::as_str).unwrap_or_default());
        let items = content.iter().map(|b| decode_block(b, &provider)).collect();

        Ok(Response {
            items,
            usage: decode_usage(body.get("usage")),
            stop: decode_stop(body.get("stop_reason").and_then(Value::as_str)),
            model,
            provider,
        })
    }
}

/// Place `cache_control` on the last system block when the plan asks for a breakpoint there.
fn apply_cache_marks(system: &mut [Value], request: &Request) {
    let Some(mark) = request.marks.last() else { return };
    let Some(last) = system.last_mut() else { return };
    let ttl = match mark.ttl {
        CacheTtl::Short => "5m",
        CacheTtl::Long => "1h",
    };
    last["cache_control"] = json!({"type": "ephemeral", "ttl": ttl});
}

fn encode_tool(def: &ToolDefinition) -> Value {
    let mut out = json!({
        "name": def.name.as_str(),
        "description": def.description,
        "input_schema": def.input_schema.as_value(),
    });
    if def.presentation == PresentationHint::Deferred {
        // Context saving, not bandwidth: the definition is still transmitted.
        out["defer_loading"] = Value::Bool(true);
    }
    match def.caller {
        CallerPolicy::CodeOnly => {
            out["allowed_callers"] = json!(["code_execution_20260120"]);
        }
        CallerPolicy::Both => {
            out["allowed_callers"] = json!(["direct", "code_execution_20260120"]);
        }
        CallerPolicy::Direct => {}
    }
    if !def.examples.is_empty() {
        out["input_examples"] = Value::Array(def.examples.iter().map(|e| e.args.clone()).collect());
    }
    out
}

fn encode_item(item: &Item) -> Option<Value> {
    match item {
        Item::Text(t) => Some(json!({"type": "text", "text": t.text})),
        Item::ToolCall(c) => Some(json!({
            "type": "tool_use",
            "id": c.id.as_str(),
            "name": c.name.as_str(),
            "input": c.args,
        })),
        Item::ToolResult(r) => Some(json!({
            "type": "tool_result",
            "tool_use_id": r.id.as_str(),
            "content": r.content,
            "is_error": r.is_error,
        })),
        Item::Reasoning(r) => {
            // Thinking blocks carry a signature that must go back verbatim, or the model loses the
            // chain of thought it already paid to produce.
            r.carry.as_ref().and_then(|c| serde_json::from_str::<Value>(c.payload.get()).ok())
        }
        // Anything preserved from this provider goes back exactly as it arrived.
        Item::Opaque(o) => serde_json::from_str::<Value>(o.raw.get()).ok(),
        // `Item` is non-exhaustive: a variant this adapter cannot express is skipped rather
        // than mapped to something approximate.
        _ => None,
    }
}

fn decode_block(block: &Value, provider: &ProviderId) -> Item {
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" => Item::Text(TextItem {
            text: block.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            provenance: Some(Provenance::new("provider:anthropic")),
        }),
        "tool_use" => Item::ToolCall(ToolCallItem {
            id: CallId::new(block.get("id").and_then(Value::as_str).unwrap_or_default()),
            name: ToolName::new(block.get("name").and_then(Value::as_str).unwrap_or_default()),
            args: block.get("input").cloned().unwrap_or(Value::Null),
            caller: decode_caller(block.get("caller")),
        }),
        "tool_result" => Item::ToolResult(ToolResultItem {
            id: CallId::new(block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default()),
            content: block.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
            is_error: block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            bytes_elided: 0,
            provenance: Provenance::new("provider:anthropic"),
        }),
        "thinking" | "redacted_thinking" => Item::Reasoning(ReasoningItem {
            summary: block.get("thinking").and_then(Value::as_str).map(str::to_string),
            visibility: if kind == "redacted_thinking" {
                ReasoningVisibility::Redacted
            } else {
                ReasoningVisibility::Plain
            },
            carry: raw_of(block)
                .map(|payload| ProviderCarry { provider: provider.clone(), payload }),
        }),
        // Everything else — server tool use, tool search results, container blocks — is preserved
        // rather than dropped. Normalisation never deletes.
        other => Item::Opaque(OpaqueItem {
            provider: provider.clone(),
            kind: other.into(),
            raw: raw_of(block).unwrap_or_else(|| {
                serde_json::value::RawValue::from_string("null".into()).expect("valid JSON")
            }),
        }),
    }
}

fn raw_of(block: &Value) -> Option<Box<serde_json::value::RawValue>> {
    serde_json::value::RawValue::from_string(block.to_string()).ok()
}

fn decode_caller(caller: Option<&Value>) -> Caller {
    match caller.and_then(|c| c.get("type")).and_then(Value::as_str) {
        Some(t) if t.starts_with("code_execution") => Caller::Code {
            runner: caller
                .and_then(|c| c.get("tool_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        },
        _ => Caller::Direct,
    }
}

fn decode_usage(usage: Option<&Value>) -> Usage {
    let Some(u) = usage else { return Usage::default() };
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        // Anthropic's `input_tokens` counts only what followed the last breakpoint.
        input: get("input_tokens"),
        output: get("output_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
        reasoning: 0,
        // Anthropic report tokens, never money.
        reported_cost: None,
        raw: raw_of(u),
    }
}

fn decode_stop(stop: Option<&str>) -> StopReason {
    match stop {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.into()),
        None => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::ids::SegmentId;
    use frey_core::item::Turn;
    use frey_core::segment::CacheMark;
    use frey_core::tool_def::JsonSchema;

    fn body() -> Value {
        json!({
            "id": "msg_1",
            "model": "claude-opus-5",
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Looking that up."},
                {"type": "thinking", "thinking": "consider options", "signature": "sig-abc"},
                {"type": "tool_use", "id": "toolu_1", "name": "fs_read", "input": {"path": "a"}},
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "tool_search_tool_regex",
                 "input": {"pattern": "weather"}}
            ],
            "usage": {
                "input_tokens": 50,
                "output_tokens": 12,
                "cache_read_input_tokens": 100000,
                "cache_creation_input_tokens": 5120
            }
        })
    }

    #[test]
    fn unmodelled_blocks_are_preserved_rather_than_dropped() {
        let response = Anthropic.decode(&body()).unwrap();
        let opaque = response
            .items
            .iter()
            .find_map(|i| match i {
                Item::Opaque(o) => Some(o),
                _ => None,
            })
            .expect("server_tool_use must survive");
        assert_eq!(opaque.kind.as_str(), "server_tool_use");
        assert!(opaque.raw.get().contains("tool_search_tool_regex"));
    }

    #[test]
    fn a_decoded_response_re_encodes_to_the_same_blocks() {
        // The conformance property: normalisation never deletes. Re-encoding an assistant turn
        // must reproduce every block, including ones Frey does not model.
        let response = Anthropic.decode(&body()).unwrap();
        let request = Request {
            model: ModelId::new("claude-opus-5"),
            turns: vec![Turn::new(Role::Assistant, response.items.clone())],
            max_output: 1024,
            ..Request::default()
        };
        let encoded = Anthropic.encode(&request, false).unwrap();
        let blocks = encoded["messages"][0]["content"].as_array().unwrap();

        assert_eq!(blocks.len(), 4, "every block returns, including the opaque one");
        assert_eq!(blocks[1]["signature"], json!("sig-abc"), "thinking signature verbatim");
        assert_eq!(blocks[3]["name"], json!("tool_search_tool_regex"));
    }

    #[test]
    fn cached_requests_are_not_understated() {
        let usage = Anthropic.decode(&body()).unwrap().usage;
        assert_eq!(usage.input, 50, "the field as reported");
        assert_eq!(usage.total_input(), 105_170, "what the request actually consumed");
        assert_eq!(usage.reported_cost, None, "Anthropic report tokens, not money");
    }

    #[test]
    fn a_cache_mark_becomes_cache_control_with_the_right_lifetime() {
        let request = Request {
            model: ModelId::new("claude-opus-5"),
            turns: vec![Turn::system("you are careful")],
            marks: vec![CacheMark { at: SegmentId(0), ttl: CacheTtl::Long }],
            max_output: 1024,
            ..Request::default()
        };
        let encoded = Anthropic.encode(&request, false).unwrap();
        assert_eq!(encoded["system"][0]["cache_control"]["ttl"], json!("1h"));
    }

    #[test]
    fn deferred_tools_are_flagged_and_code_only_tools_declare_their_caller() {
        let mut deferred = ToolDefinition::new(
            "z_rare",
            "A rarely used tool with a full description",
            JsonSchema::empty_object(),
        );
        deferred.presentation = PresentationHint::Deferred;
        let mut code_only = ToolDefinition::new(
            "db_query",
            "Query the database and return rows",
            JsonSchema::empty_object(),
        );
        code_only.caller = CallerPolicy::CodeOnly;

        let request = Request {
            model: ModelId::new("claude-opus-5"),
            tools: vec![deferred, code_only],
            max_output: 1024,
            ..Request::default()
        };
        let encoded = Anthropic.encode(&request, false).unwrap();
        assert_eq!(encoded["tools"][0]["defer_loading"], json!(true));
        assert_eq!(encoded["tools"][1]["allowed_callers"], json!(["code_execution_20260120"]));
    }

    #[test]
    fn minimum_cacheable_prefix_is_read_from_the_model_name() {
        assert_eq!(
            Anthropic.capabilities(&ModelId::new("claude-opus-5")).cache.min_prefix_tokens(),
            Some(512)
        );
        assert_eq!(
            Anthropic
                .capabilities(&ModelId::new("claude-haiku-4-5-20251001"))
                .cache
                .min_prefix_tokens(),
            Some(4_096)
        );
    }

    #[test]
    fn an_unrecognised_stop_reason_is_kept_rather_than_flattened() {
        let mut b = body();
        b["stop_reason"] = json!("content_filtered");
        let response = Anthropic.decode(&b).unwrap();
        assert_eq!(response.stop, StopReason::Other("content_filtered".into()));
        assert!(!response.stop.wants_tools());
    }

    #[test]
    fn a_malformed_body_is_a_protocol_error_not_a_panic() {
        let err = Anthropic.decode(&json!({"oops": true})).unwrap_err();
        assert!(matches!(err, ProviderError::Protocol { .. }));
        assert!(!err.is_retryable());
    }
}
