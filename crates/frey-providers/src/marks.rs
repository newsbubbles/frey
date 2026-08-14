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
    let openrouter = crate::openrouter::OpenRouter;

    vec![
        measure("anthropic", &anthropic, "claude-opus-5"),
        measure("openai", &openai, "gpt-5"),
        measure("openrouter", &openrouter, "anthropic/claude-opus-5"),
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
    let tool = ToolDefinition::new(
        "fs_read",
        "Read a file from the workspace and return its contents",
        JsonSchema::empty_object(),
    );
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
