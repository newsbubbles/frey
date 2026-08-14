//! Does the cache plan actually reach the wire?
//!
//! This module exists because the answer turned out to be *on one dialect of three*, and nothing in
//! the project could have told you. The planner is a pure function with thirty tests; the adapters
//! are pure functions with their own; and the question of whether a mark the planner placed appears
//! in the bytes an adapter produces fell in the gap between them.
//!
//! The consequence was not small. `openrouter.rs` contains no reference to `cache_control` or to
//! `request.marks`, declares `CacheSupport::Automatic { explicit_available: false }`, and therefore
//! gets a breakpoint budget of zero — so on the only dialect Frey's only real caller uses, every
//! cache plan was empty by construction, and had been for every session ever run.
//!
//! That is defensible **as designed**: OpenRouter caches automatically, there is no breakpoint to
//! place, and `provider_caches_automatically` says so truthfully. What was not defensible was that
//! nothing said it out loud, while the README's opening paragraph implied the planner governed
//! every provider.
//!
//! So: measure it. [`survey`] runs one synthetic request through every dialect and counts the marks
//! that come out the other side. `frey doctor` prints the table, and a test asserts the invariant
//! that matters — **a dialect that is handed marks must emit them** — which is the general form of
//! the specific bug, and the shape of check that would have caught it.

use frey_core::ids::{ModelId, ToolName};
use frey_core::item::{Item, Role, Turn};
use frey_core::provider::Request;
use frey_core::provider_caps::CacheSupport;
use frey_core::segment::{CacheMark, CacheTtl};
use frey_core::tool_def::{JsonSchema, ToolDefinition};

use crate::dialect::Dialect;

/// What one dialect does with a cache plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkSupport {
    /// The adapter.
    pub provider: &'static str,
    /// How many breakpoints the planner is allowed to place for this model.
    pub budget: u8,
    /// How many marks the request carried in.
    pub planned: usize,
    /// How many `cache_control` markers came out in the encoded body.
    pub realised: usize,
    /// Whether the provider caches without being asked.
    pub automatic: bool,
}

impl MarkSupport {
    /// A one-line answer to "does my cache plan do anything here".
    #[must_use]
    pub fn summary(&self) -> String {
        match (self.realised, self.automatic) {
            (0, true) => "no breakpoints; the provider caches the prefix itself".into(),
            (0, false) => "no breakpoints and no automatic caching: nothing is cached".into(),
            (n, _) => format!("{n} breakpoint(s) placed on the wire"),
        }
    }

    /// The invariant: a dialect handed marks must put them somewhere.
    ///
    /// A dialect with no budget is handed none and passes trivially, which is correct — the claim
    /// under test is *if you accept breakpoints, they reach the request*, not *you accept them*.
    #[must_use]
    pub fn is_honest(&self) -> bool {
        self.planned == 0 || self.realised > 0
    }
}

/// Run one representative request through every built-in dialect and count what survives.
///
/// The request is deliberately ordinary: a tool block, a system prompt, and two turns, with a mark
/// at the end of every one of them. Nothing exotic — the point is what a normal agent produces.
#[must_use]
pub fn survey() -> Vec<MarkSupport> {
    let anthropic = crate::anthropic::Anthropic;
    let openai = crate::openai::OpenAiResponses;
    let openrouter = crate::openrouter::OpenRouter::new();
    let openrouter_explicit = crate::openrouter::OpenRouter::new().with_explicit_cache();

    vec![
        measure("anthropic", &anthropic, "claude-opus-5"),
        measure("openai", &openai, "gpt-5"),
        measure("openrouter", &openrouter, "anthropic/claude-opus-5"),
        measure("openrouter+explicit", &openrouter_explicit, "anthropic/claude-opus-5"),
    ]
}

fn measure(name: &'static str, dialect: &dyn Dialect, model: &str) -> MarkSupport {
    let model = ModelId::new(model);
    let caps = dialect.capabilities(&model);
    let budget = caps.cache.breakpoint_budget();

    let mut request = representative(model);
    // Only as many marks as the planner would ever place. Handing a dialect four breakpoints when
    // its budget is one would be measuring a request the loop cannot produce.
    request.marks.truncate(budget as usize);
    let planned = request.marks.len();

    let realised =
        dialect.encode(&request, false).map(|body| count_cache_control(&body)).unwrap_or_default();

    MarkSupport {
        provider: name,
        budget,
        planned,
        realised,
        automatic: matches!(caps.cache, CacheSupport::Automatic { .. }),
    }
}

/// A tool block, a system prompt and two turns, with a breakpoint at the end of each.
fn representative(model: ModelId) -> Request {
    // **Not `ToolDefinition::new` alone.** Its `presentation` defaults to `Deferred`, and Anthropic
    // reject `cache_control` on a deferred tool — so a survey built from the default would be
    // measuring the one tool shape that cannot carry the mark it is checking for, and this module's
    // own test would have asserted the forbidden pairing was correct.
    let tool = ToolDefinition {
        presentation: frey_core::tool_def::PresentationHint::Always,
        ..ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents",
            JsonSchema::empty_object(),
        )
    };
    Request {
        model,
        tools: vec![tool],
        turns: vec![
            Turn::system("You are a careful assistant."),
            Turn::user("What changed this week?"),
            Turn::new(
                Role::Assistant,
                vec![Item::ToolCall(frey_core::item::ToolCallItem {
                    id: frey_core::ids::CallId::new("c1"),
                    name: ToolName::new("fs_read"),
                    args: serde_json::json!({"path": "CHANGELOG.md"}),
                    caller: frey_core::item::Caller::Direct,
                })],
            ),
        ],
        marks: vec![
            CacheMark { at: frey_core::ids::SegmentId(0), ttl: CacheTtl::Long },
            CacheMark { at: frey_core::ids::SegmentId(1), ttl: CacheTtl::Long },
            CacheMark { at: frey_core::ids::SegmentId(2), ttl: CacheTtl::Short },
            CacheMark { at: frey_core::ids::SegmentId(3), ttl: CacheTtl::Short },
        ],
        max_output: 1_024,
        ..Request::default()
    }
}

/// Count `cache_control` keys anywhere in a body.
///
/// Structural rather than a substring search over the serialised text, so a tool *named*
/// `cache_control` cannot inflate the count.
fn count_cache_control(body: &serde_json::Value) -> usize {
    match body {
        serde_json::Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(count_cache_control).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dialect_that_accepts_breakpoints_actually_emits_them() {
        // The general form of the bug. A dialect may legitimately decline to take breakpoints — that
        // is what a zero budget means — but one that declares a budget and then drops the marks is
        // charging for a feature it does not have, silently, with the only symptom being the bill.
        for support in survey() {
            assert!(
                support.is_honest(),
                "{} claims {} breakpoint(s) and emitted {}: {support:?}",
                support.provider,
                support.planned,
                support.realised
            );
        }
    }

    #[test]
    fn anthropic_realises_the_whole_plan_and_not_just_its_last_mark() {
        // Four breakpoints is the entire reason `max_breakpoints: 4` exists. The adapter used to
        // take `marks.last()`, so an Opus plan that spread four across the tool block, the system
        // prompt and two turns arrived as one — three quarters of the planner's work discarded
        // between the plan and the wire, with the plan still reporting four.
        let anthropic = survey().into_iter().find(|s| s.provider == "anthropic").expect("present");
        assert_eq!(anthropic.budget, 4);
        assert_eq!(anthropic.planned, 4);
        assert_eq!(anthropic.realised, 4, "every mark must land: {anthropic:?}");
    }

    #[test]
    fn the_tool_block_is_marked_when_the_plan_says_so() {
        // Named separately because the doc comment claimed it for months while `encode_tool` emitted
        // no `cache_control` at all — and the tool block is the largest and most stable segment in
        // a typical prompt, so it is the mark worth the most.
        let dialect = crate::anthropic::Anthropic;
        let mut request = representative(ModelId::new("claude-opus-5"));
        request.marks = vec![CacheMark { at: frey_core::ids::SegmentId(0), ttl: CacheTtl::Long }];
        let body = dialect.encode(&request, false).expect("encodes");
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], serde_json::json!("1h"), "{body}");
    }

    #[test]
    fn a_breakpoint_through_the_router_lands_on_a_content_part_not_beside_one() {
        // `cache_control` is documented on a content *part*. Hung beside a string `content` it is a
        // field the upstream has no reason to read: accepted, ignored, and indistinguishable from
        // working — which is the shape of failure this whole module exists to catch.
        let dialect = crate::openrouter::OpenRouter::new().with_explicit_cache();
        let mut request = representative(ModelId::new("anthropic/claude-opus-5"));
        request.marks = vec![CacheMark { at: frey_core::ids::SegmentId(1), ttl: CacheTtl::Long }];

        let body = dialect.encode(&request, false).expect("encodes");
        let system = &body["messages"][0];
        assert!(system["content"].is_array(), "a marked message becomes parts: {body}");
        assert_eq!(system["content"][0]["type"], serde_json::json!("text"));
        assert_eq!(system["content"][0]["cache_control"]["ttl"], serde_json::json!("1h"));

        // And an unmarked message keeps the plain string form, because the long tail of
        // OpenAI-compatible servers accepts nothing else.
        assert!(body["messages"][1]["content"].is_string(), "{body}");
    }

    #[test]
    fn a_breakpoint_is_refused_on_a_deferred_tool_rather_than_producing_a_400() {
        // `PresentationHint::Deferred` is the default, so `ToolDefinition::new` gives it to you,
        // and Anthropic answer 400 to `cache_control` on the same tool. The trigger is narrower
        // than it sounds — the tool block has to be the last cacheable segment, which happens
        // whenever an agent has no system prompt or whose system prompt churned — and the symptom
        // is a run that will not start at all.
        let dialect = crate::anthropic::Anthropic;
        let mut request = representative(ModelId::new("claude-opus-5"));
        request.tools =
            vec![ToolDefinition::new("fs_read", "read a file", JsonSchema::empty_object())];
        request.marks = vec![CacheMark { at: frey_core::ids::SegmentId(0), ttl: CacheTtl::Long }];

        let body = dialect.encode(&request, false).expect("encodes");
        assert_eq!(body["tools"][0]["defer_loading"], serde_json::json!(true), "the default");
        assert!(
            body["tools"][0].get("cache_control").is_none(),
            "the inference gives way to the instruction: {body}"
        );
    }

    #[test]
    fn a_mark_survives_an_empty_system_prompt() {
        // The old code returned early when `system` was empty, so an agent with no system prompt —
        // which is every `Agent::new(..)` that never calls `.system(..)` — got no caching at all.
        let dialect = crate::anthropic::Anthropic;
        let mut request = representative(ModelId::new("claude-opus-5"));
        request.turns.remove(0);
        request.marks = vec![CacheMark { at: frey_core::ids::SegmentId(0), ttl: CacheTtl::Long }];
        let body = dialect.encode(&request, false).expect("encodes");
        assert_eq!(count_cache_control(&body), 1, "{body}");
    }

    #[test]
    fn openrouter_places_breakpoints_only_when_asked_and_only_where_documented() {
        // `profiles::openrouter_explicit()` had zero callers outside its own test — a profile
        // describing a capability no dialect ever returned. This is the switch that makes it real,
        // and it is opt-in because the comment it sits beside stays true: a hardcoded table of
        // upstream quirks rots faster than the release cycle. One documented family, chosen by a
        // caller, is a different thing from a table this crate maintains for everyone.
        let explicit = crate::openrouter::OpenRouter::new().with_explicit_cache();

        let anthropic_family = measure("or", &explicit, "anthropic/claude-opus-5");
        assert_eq!(anthropic_family.budget, 4);
        // Three, not four. The plan's tool-block mark is deliberately refused on this dialect —
        // Chat Completions tools have no content part to carry `cache_control`, and emitting it on
        // the function wrapper would be a marker the upstream never reads with this survey counting
        // it as placed. See `apply_cache_marks`.
        assert_eq!(anthropic_family.planned, 4);
        assert_eq!(anthropic_family.realised, 3, "{anthropic_family:?}");
        assert!(anthropic_family.is_honest(), "some of the plan reaches the wire");

        // Everything else keeps the automatic answer whether or not the switch is on. Sending
        // `cache_control` to an upstream that ignores or rejects it fails invisibly from here.
        let elsewhere = measure("or", &explicit, "meta-llama/llama-3.1-8b-instruct");
        assert_eq!(elsewhere.budget, 0);
        assert_eq!(elsewhere.realised, 0);
    }

    #[test]
    fn a_breakpoint_lands_on_the_message_the_planner_chose_and_not_the_one_at_that_index() {
        // The trap in this encoder, and the reason `messages_for` exists. Turns do not map
        // one-for-one to Chat Completions messages: a turn carrying two tool results explodes into
        // two messages, so indexing `messages` by turn number puts the breakpoint somewhere the
        // planner never chose — caching a prefix nobody reasoned about, and reporting success.
        use frey_core::ids::CallId;
        use frey_core::item::ToolResultItem;

        let dialect = crate::openrouter::OpenRouter::new().with_explicit_cache();
        let results = Turn::new(
            Role::User,
            vec![
                Item::ToolResult(ToolResultItem {
                    id: CallId::new("c1"),
                    content: "one".into(),
                    is_error: false,
                    bytes_elided: 0,
                    provenance: frey_core::taint::Provenance::new("t"),
                }),
                Item::ToolResult(ToolResultItem {
                    id: CallId::new("c2"),
                    content: "two".into(),
                    is_error: false,
                    bytes_elided: 0,
                    provenance: frey_core::taint::Provenance::new("t"),
                }),
            ],
        );

        let request = Request {
            model: ModelId::new("anthropic/claude-opus-5"),
            tools: Vec::new(),
            turns: vec![Turn::system("stable"), results, Turn::user("and now?")],
            // No tools, so segment 0 is the system turn and segment 2 is the last user turn.
            marks: vec![CacheMark { at: frey_core::ids::SegmentId(1), ttl: CacheTtl::Short }],
            max_output: 512,
            ..Request::default()
        };

        let body = dialect.encode(&request, false).expect("encodes");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4, "two tool results are two messages: {body}");
        // The marker lives on the content part now, not beside it — see `mark_message`.
        let marked = |m: &serde_json::Value| m["content"][0].get("cache_control").is_some();
        assert!(!marked(&messages[0]), "not the system message");
        assert!(
            marked(&messages[2]),
            "the mark belongs on the last message that turn produced: {body}"
        );
        assert!(!marked(&messages[3]));
    }

    #[test]
    fn openrouter_is_recorded_as_placing_nothing_rather_than_pretending() {
        // Not a failure. It is the honest state of the dialect the only real caller uses, and the
        // whole point of this module is that it be visible rather than inferred from a grep.
        let openrouter =
            survey().into_iter().find(|s| s.provider == "openrouter").expect("present");
        assert_eq!(openrouter.budget, 0);
        assert_eq!(openrouter.planned, 0);
        assert_eq!(openrouter.realised, 0);
        assert!(openrouter.automatic);
        assert!(openrouter.summary().contains("the provider caches the prefix itself"));
    }
}
