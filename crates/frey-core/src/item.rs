//! The conversation model.
//!
//! Frey's conversation is a list of **items**, not a list of messages. That is the single
//! highest-leverage decision in the codebase (ADR-0003), and it is forced by the providers
//! themselves: OpenAI's Responses API is item-based, Anthropic's content blocks are item-like, and
//! reasoning state must be round-tripped verbatim or the model loses its chain of thought and you
//! pay to regenerate it.
//!
//! A message-shaped core would force lossy conversion at exactly the place lossiness costs money.
//! Message-shaped providers are therefore a *projection* of this model, never the reverse.
//!
//! # The `Opaque` rule
//!
//! Anything a provider emits that Frey does not model is preserved as [`Item::Opaque`], byte for
//! byte, and replayed unchanged. Normalisation never deletes. The conformance test that keeps this
//! honest lives with each provider adapter.

use serde_json::value::RawValue;
use smol_str::SmolStr;

use crate::ids::{AgentId, CallId, ProviderId, ToolName};
use crate::taint::Provenance;

/// Who produced a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The operator or end user.
    User,
    /// The model.
    Assistant,
    /// Operator-authored instructions. Kept distinct from `User` because it is the stable cache
    /// prefix and has different taint (`High` integrity).
    System,
}

/// How a tool call was made. Mirrors Anthropic's `caller` field and generalises it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Caller {
    /// The model called the tool directly.
    Direct,
    /// A code-mode script called it. `runner` correlates back to the code execution block.
    Code {
        /// The id of the code execution that made the call.
        runner: SmolStr,
    },
    /// The frontend executed it (AG-UI).
    Frontend,
    /// A sub-agent called it.
    SubAgent {
        /// Which agent.
        agent: AgentId,
    },
}

impl Caller {
    /// Whether the call came from inside a code-mode sandbox.
    #[must_use]
    pub fn is_code(&self) -> bool {
        matches!(self, Self::Code { .. })
    }
}

/// What a reasoning item exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningVisibility {
    /// The text is readable.
    Plain,
    /// The provider redacted it.
    Redacted,
    /// The provider encrypted it and expects it back verbatim.
    Encrypted,
}

/// A media payload. Kept separate from text because every provider treats them differently for
/// caching: on Anthropic, adding or removing *any* image invalidates the system and message cache
/// segments, so the cache planner needs to see media as media.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Media {
    /// An image.
    Image {
        /// MIME type, e.g. `image/png`.
        mime: SmolStr,
        /// Where the bytes are.
        source: MediaSource,
        /// Provider-specific detail hint. Must match exactly for a cache hit.
        detail: Option<SmolStr>,
    },
    /// Audio.
    Audio {
        /// MIME type.
        mime: SmolStr,
        /// Where the bytes are.
        source: MediaSource,
    },
    /// A document, e.g. a PDF.
    Document {
        /// MIME type.
        mime: SmolStr,
        /// Where the bytes are.
        source: MediaSource,
        /// Display name, when the provider supports one.
        name: Option<SmolStr>,
    },
}

/// Where media bytes live.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MediaSource {
    /// Base64 data, inline.
    Inline {
        /// Base64-encoded bytes.
        data: String,
    },
    /// A URL the provider will fetch.
    Url {
        /// The location.
        url: String,
    },
    /// A handle to something already uploaded to the provider.
    FileId {
        /// The provider's identifier.
        id: SmolStr,
    },
}

/// Provider-owned state that must survive a round trip untouched.
///
/// OpenAI's `encrypted_content` on reasoning items and Anthropic's thinking signatures both live
/// here. Dropping one is silent and expensive, so this type has no constructor that discards it and
/// no `Default`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderCarry {
    /// Which provider owns this blob. Replaying it to a different provider is a bug.
    pub provider: ProviderId,
    /// The blob, exactly as received.
    pub payload: Box<RawValue>,
}

/// Text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextItem {
    /// The text.
    pub text: String,
    /// Where it came from. Absent for operator-authored text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// The model's reasoning.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReasoningItem {
    /// A human-readable summary, when the provider gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What is exposed.
    pub visibility: ReasoningVisibility,
    /// Provider state to replay verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carry: Option<ProviderCarry>,
}

/// The model asked for a tool to run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallItem {
    /// Correlation id.
    pub id: CallId,
    /// Which tool.
    pub name: ToolName,
    /// The arguments, unvalidated.
    pub args: serde_json::Value,
    /// How the call was made.
    pub caller: Caller,
}

/// What a tool returned.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolResultItem {
    /// Correlation id, matching the call.
    pub id: CallId,
    /// The rendered result the model sees.
    pub content: String,
    /// Whether this represents a failure. The model is told either way; this drives presentation.
    pub is_error: bool,
    /// How many bytes were cut. Non-zero means the model should be told how to get the rest —
    /// silent truncation produces bugs nobody can diagnose.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes_elided: u64,
    /// Where the result came from.
    pub provenance: Provenance,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A capability entered the conversation part-way through: a deferred tool was discovered, a skill
/// was loaded, or a catalog changed.
///
/// This is an item rather than a side effect because it costs tokens and must appear in the journal
/// at the exact position it entered the context, or replay diverges.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryItem {
    /// What was found.
    pub found: Vec<ToolName>,
    /// How it was found.
    pub via: SmolStr,
    /// Roughly what it cost to inject.
    pub est_tokens: u32,
}

/// Something the provider emitted that Frey does not model.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpaqueItem {
    /// Which provider produced it.
    pub provider: ProviderId,
    /// The provider's own discriminator, for debugging and for round-trip fidelity.
    pub kind: SmolStr,
    /// The block, exactly as received.
    pub raw: Box<RawValue>,
}

// `RawValue` has no `PartialEq`, and that is the right default for it: comparing arbitrary JSON
// structurally is usually wrong. Here it is exactly right, because the property these types exist
// to guarantee is *byte-for-byte* fidelity, so comparing the raw text is the stronger check.
impl PartialEq for ProviderCarry {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider && self.payload.get() == other.payload.get()
    }
}
impl Eq for ProviderCarry {}

impl PartialEq for OpaqueItem {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.kind == other.kind
            && self.raw.get() == other.raw.get()
    }
}
impl Eq for OpaqueItem {}

/// One addressable unit of conversation state.
///
/// # Why this enum is externally tagged
///
/// `#[serde(tag = "...")]` makes serde buffer the whole value through its internal `Content` type
/// before dispatching on the tag, and that buffer cannot represent `RawValue`'s newtype-struct
/// trick. Internal or adjacent tagging here therefore breaks [`Item::Opaque`] and
/// [`ProviderCarry`] at runtime — the exact byte-fidelity guarantee this module exists to provide.
/// External tagging passes the real deserializer through, so raw payloads survive.
/// `enum_is_externally_tagged_so_raw_payloads_survive` is the regression test.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Item {
    /// Text.
    Text(TextItem),
    /// An image, audio clip, or document.
    Media(Media),
    /// The model's reasoning.
    Reasoning(ReasoningItem),
    /// A request to run a tool.
    ToolCall(ToolCallItem),
    /// A tool's output.
    ToolResult(ToolResultItem),
    /// A capability entered the context.
    Discovery(DiscoveryItem),
    /// Preserved verbatim.
    Opaque(OpaqueItem),
}

impl Item {
    /// Plain text from the operator.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextItem { text: text.into(), provenance: None })
    }

    /// Text from somewhere outside.
    pub fn text_from(text: impl Into<String>, provenance: Provenance) -> Self {
        Self::Text(TextItem { text: text.into(), provenance: Some(provenance) })
    }

    /// Whether this item carries provider state that must be replayed verbatim. Dropping one of
    /// these is the classic, silent, expensive mistake.
    #[must_use]
    pub fn must_round_trip(&self) -> bool {
        match self {
            Self::Reasoning(r) => r.carry.is_some(),
            Self::Opaque(_) => true,
            _ => false,
        }
    }

    /// Whether this item is media, which several providers treat as a cache-invalidating change.
    #[must_use]
    pub fn is_media(&self) -> bool {
        matches!(self, Self::Media(_))
    }

    /// The correlation id, for the two item kinds that have one.
    #[must_use]
    pub fn call_id(&self) -> Option<&CallId> {
        match self {
            Self::ToolCall(c) => Some(&c.id),
            Self::ToolResult(r) => Some(&r.id),
            _ => None,
        }
    }
}

/// One side of the conversation: a contiguous run of items from a single role.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    /// Who produced it.
    pub role: Role,
    /// The items, in order.
    pub items: Vec<Item>,
}

impl Turn {
    /// A turn from `role` containing `items`.
    pub fn new(role: Role, items: impl IntoIterator<Item = Item>) -> Self {
        Self { role, items: items.into_iter().collect() }
    }

    /// A user turn containing a single text item.
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, [Item::text(text)])
    }

    /// A system turn containing a single text item.
    pub fn system(text: impl Into<String>) -> Self {
        Self::new(Role::System, [Item::text(text)])
    }

    /// Every tool call in this turn.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCallItem> {
        self.items.iter().filter_map(|i| match i {
            Item::ToolCall(c) => Some(c),
            _ => None,
        })
    }

    /// Whether any item must be replayed verbatim.
    #[must_use]
    pub fn has_provider_carry(&self) -> bool {
        self.items.iter().any(Item::must_round_trip)
    }

    /// Whether this turn contains media.
    #[must_use]
    pub fn has_media(&self) -> bool {
        self.items.iter().any(Item::is_media)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[test]
    fn opaque_items_survive_a_round_trip_byte_for_byte() {
        // Deliberately awkward: unusual key order, a float that would change under reformatting,
        // deep nesting, and a unicode escape.
        let original = r#"{"z":1,"a":{"nested":[1.5000,{"deep":true}]},"u":"é"}"#;
        let item = Item::Opaque(OpaqueItem {
            provider: ProviderId::new("anthropic"),
            kind: SmolStr::new("server_tool_use"),
            raw: raw(original),
        });

        let encoded = serde_json::to_string(&item).unwrap();
        let decoded: Item = serde_json::from_str(&encoded).unwrap();

        let Item::Opaque(o) = &decoded else { panic!("expected an opaque item") };
        assert_eq!(o.raw.get(), original, "opaque payloads must not be reformatted");
        assert_eq!(decoded, item);
    }

    #[test]
    fn provider_carry_survives_a_round_trip() {
        let carry_payload = r#"{"encrypted_content":"gAAAAAB…","signature":"abc"}"#;
        let item = Item::Reasoning(ReasoningItem {
            summary: Some("considered three options".into()),
            visibility: ReasoningVisibility::Encrypted,
            carry: Some(ProviderCarry {
                provider: ProviderId::new("openai"),
                payload: raw(carry_payload),
            }),
        });
        assert!(item.must_round_trip());

        let decoded: Item = serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        let Item::Reasoning(r) = &decoded else { panic!("expected reasoning") };
        assert_eq!(r.carry.as_ref().unwrap().payload.get(), carry_payload);
    }

    #[test]
    fn reasoning_without_carry_does_not_claim_to_need_round_tripping() {
        let item = Item::Reasoning(ReasoningItem {
            summary: Some("thought about it".into()),
            visibility: ReasoningVisibility::Plain,
            carry: None,
        });
        assert!(!item.must_round_trip());
    }

    #[test]
    fn a_whole_turn_round_trips() {
        let turn = Turn::new(
            Role::Assistant,
            [
                Item::text("I'll look that up."),
                Item::Reasoning(ReasoningItem {
                    summary: None,
                    visibility: ReasoningVisibility::Encrypted,
                    carry: Some(ProviderCarry {
                        provider: ProviderId::new("openai"),
                        payload: raw(r#""opaque-blob""#),
                    }),
                }),
                Item::ToolCall(ToolCallItem {
                    id: CallId::new("call_1"),
                    name: ToolName::new("fs_read"),
                    args: serde_json::json!({"path": "src/main.rs"}),
                    caller: Caller::Code { runner: "srvtoolu_9".into() },
                }),
                Item::Opaque(OpaqueItem {
                    provider: ProviderId::new("openai"),
                    kind: "web_search_call".into(),
                    raw: raw(r#"{"status":"completed"}"#),
                }),
            ],
        );

        let decoded: Turn = serde_json::from_str(&serde_json::to_string(&turn).unwrap()).unwrap();
        assert_eq!(decoded, turn);
        assert!(decoded.has_provider_carry());
        assert_eq!(decoded.tool_calls().count(), 1);
        assert!(decoded.tool_calls().next().unwrap().caller.is_code());
    }

    #[test]
    fn tool_results_report_how_much_they_hid() {
        let item = Item::ToolResult(ToolResultItem {
            id: CallId::new("call_1"),
            content: "first 4 KiB…".into(),
            is_error: false,
            bytes_elided: 1_048_576,
            provenance: Provenance::new("tool:shell"),
        });
        let encoded = serde_json::to_string(&item).unwrap();
        assert!(encoded.contains("bytes_elided"), "truncation must never be silent");

        // The common case stays compact on the wire.
        let untruncated = Item::ToolResult(ToolResultItem {
            id: CallId::new("call_2"),
            content: "ok".into(),
            is_error: false,
            bytes_elided: 0,
            provenance: Provenance::new("tool:shell"),
        });
        assert!(!serde_json::to_string(&untruncated).unwrap().contains("bytes_elided"));
    }

    #[test]
    fn enum_is_externally_tagged_so_raw_payloads_survive() {
        // Guard for a landmine. `#[serde(tag = "...")]` buffers the value through serde's internal
        // `Content` type before dispatching on the tag, and that buffer cannot represent
        // `RawValue`. Adding internal or adjacent tagging to `Item` compiles cleanly and then fails
        // at runtime with "invalid type: newtype struct, expected any valid JSON value" — silently
        // destroying the byte-fidelity guarantee. This test fails immediately if anyone tries.
        let item = Item::Opaque(OpaqueItem {
            provider: ProviderId::new("anthropic"),
            kind: "server_tool_use".into(),
            raw: raw(r#"{"nested":{"deep":[1,2,3]}}"#),
        });
        let encoded = serde_json::to_string(&item).unwrap();
        assert!(
            encoded.starts_with(r#"{"opaque":"#),
            "Item must stay externally tagged; got {encoded}"
        );
        assert_eq!(serde_json::from_str::<Item>(&encoded).unwrap(), item);
    }

    #[test]
    fn media_is_visible_to_the_cache_planner() {
        let turn = Turn::new(
            Role::User,
            [
                Item::text("what is this?"),
                Item::Media(Media::Image {
                    mime: "image/png".into(),
                    source: MediaSource::Url { url: "https://example.test/x.png".into() },
                    detail: Some("high".into()),
                }),
            ],
        );
        assert!(turn.has_media(), "adding an image invalidates cache segments on some providers");
    }
}
