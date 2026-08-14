//! The event bus.
//!
//! One ordered stream of events serves four consumers: the AG-UI frontend protocol, A2A task
//! updates, OpenTelemetry spans, and the run journal. Building the internal bus in AG-UI's shape
//! (ADR-0015) means the harness gets a frontend protocol with no adapter, only a serialiser.
//!
//! # The backpressure rule
//!
//! Under load, **presentation events may be dropped; semantic events may not.** A dropped token
//! delta makes the UI stutter. A dropped tool call makes the transcript a lie, and replay diverges.
//! [`Event::is_droppable`] is the single point where that distinction lives, and it is tested
//! exhaustively so a new variant cannot quietly become droppable.
//!
//! Dropped deltas are counted and reported at the end of a run, so "the UI looked laggy" is
//! diagnosable rather than folklore.

use smol_str::SmolStr;

use crate::error::{NeedsInput, ToolError};
use crate::ids::{AgentPath, CallId, RunId, SeqId, ToolName, TurnId};
use crate::usage::{CostEstimate, Usage, UsageTotals};

/// Something worth telling a warning about. These are the framework's user-visible diagnostics, and
/// they should read like good compiler errors: what happened, what it costs, what to do.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Warning {
    /// A segment that should have been stable changed, so its cached prefix was thrown away.
    CacheChurn {
        /// Which segment, by its human label.
        segment: SmolStr,
        /// How many tokens are being rewritten each turn.
        tokens: u32,
        /// What to do about it.
        advice: SmolStr,
    },
    /// The prefix is shorter than the model's minimum cacheable length, so caching is doing
    /// nothing — silently, because providers do not report this.
    BelowMinPrefix {
        /// What the prefix measures.
        have: u32,
        /// What the model needs.
        need: u32,
    },
    /// A requested capability is missing, and Frey fell back.
    Degraded {
        /// What was wanted.
        capability: SmolStr,
        /// What is happening instead.
        fallback: SmolStr,
    },
    /// The router served this call from a different provider than the last one, which changes the
    /// tokenizer, the price, and the cache.
    RouteChanged {
        /// The previous provider.
        from: SmolStr,
        /// The new one.
        to: SmolStr,
    },
    /// The context budget is nearly exhausted and eviction has begun.
    ///
    /// Expressed as a whole percentage rather than a float so that events stay `Eq` and can be
    /// compared exactly in replay assertions.
    BudgetPressure {
        /// Percentage of the budget used, `0..=100`.
        used_percent: u8,
        /// What was evicted or summarised.
        action: SmolStr,
    },
    /// A turn added more content blocks than the provider looks back through, so the next request
    /// will miss the cache entirely — with no error from the provider.
    LookbackExceeded {
        /// How many blocks the turn added.
        blocks: u32,
        /// How far back the provider searches.
        limit: u32,
    },
    /// Presentation events were dropped to keep up.
    EventsDropped {
        /// How many.
        count: u64,
    },
    /// One model response asked for more tool calls than the run permits in a single turn, so the
    /// excess was refused rather than executed.
    ///
    /// A turn limit bounds how many times the model is *called*; it does nothing about how much
    /// work a single response demands. A weak model that loses the thread can emit hundreds of
    /// calls in one response, and executing them is a runaway with real side effects — not merely
    /// a large bill. `meta-llama/llama-3.1-8b-instruct` produced roughly 145 in one response during
    /// the first live session.
    ToolCallsCapped {
        /// How many the model asked for.
        requested: u32,
        /// How many were permitted.
        cap: u32,
    },
}

/// The doc comment on this type says these "should read like good compiler errors: what happened,
/// what it costs, what to do" — and until this impl existed there was no way to read them at all.
///
/// A caller who wanted to surface a warning had to write a match over eight `#[non_exhaustive]`
/// variants, which downstream crates cannot do exhaustively, so in practice every caller either
/// used `{:?}` or dropped them. Both defeat the point: "nothing degrades quietly" is only true if
/// the diagnostic reaches a person.
impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CacheChurn { segment, tokens, advice } => write!(
                f,
                "cache churn: `{segment}` changed and took {tokens} cached tokens with it, every \
                 turn. {advice}"
            ),
            Self::BelowMinPrefix { have, need } => write!(
                f,
                "caching is doing nothing: the prefix is {have} tokens and this model needs {need}. \
                 Providers do not report this, so nothing else will tell you."
            ),
            Self::Degraded { capability, fallback } => {
                write!(f, "no {capability} here; {fallback}")
            }
            Self::RouteChanged { from, to } => write!(
                f,
                "the router moved this call from {from} to {to}, which changes the tokenizer, the \
                 price, and whether the cache still exists"
            ),
            Self::BudgetPressure { used_percent, action } => {
                write!(f, "context {used_percent}% full; {action}")
            }
            Self::LookbackExceeded { blocks, limit } => write!(
                f,
                "that turn added {blocks} blocks and this provider looks back {limit}, so the next \
                 request misses the cache entirely — with no error from anyone"
            ),
            Self::EventsDropped { count } => {
                write!(f, "{count} presentation event(s) dropped to keep up")
            }
            Self::ToolCallsCapped { requested, cap } => write!(
                f,
                "the model asked for {requested} tool calls in one response and {cap} were \
                 permitted; the rest were refused, not silently dropped"
            ),
            // Deliberately no catch-all. `#[non_exhaustive]` does not bind this crate, so a new
            // variant breaks this match at compile time — which is the point. A warning nobody
            // wrote a sentence for is a warning that reaches a person as `Debug` output.
        }
    }
}

/// Something that happened during a run.
/// `EventKind` is externally tagged for the same reason [`crate::item::Item`] is: it carries
/// [`Usage`], which holds the provider's raw usage object, and internal tagging would corrupt it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// A run began.
    RunStarted {
        /// Which run.
        run: RunId,
    },
    /// A turn began.
    TurnStarted {
        /// Which turn.
        turn: TurnId,
    },
    /// A chunk of assistant text. **Droppable.**
    TextDelta {
        /// The chunk.
        text: String,
    },
    /// A chunk of reasoning. **Droppable.**
    ReasoningDelta {
        /// The chunk.
        text: String,
    },
    /// A tool call started.
    ToolCallStarted {
        /// Correlation id.
        call: CallId,
        /// Which tool.
        name: ToolName,
        /// A truncated rendering of the arguments, for display. The full arguments are in the
        /// journal.
        args_preview: String,
    },
    /// A tool call finished successfully.
    ToolCallFinished {
        /// Correlation id.
        call: CallId,
        /// How long it took, in milliseconds.
        millis: u64,
        /// How many bytes were hidden from the model.
        bytes_elided: u64,
    },
    /// A tool call failed or was denied.
    ToolCallFailed {
        /// Correlation id.
        call: CallId,
        /// What went wrong. Carries all three audiences; the serialiser picks the right one.
        error: ToolError,
    },
    /// Capabilities entered the context.
    Discovered {
        /// What was found.
        found: Vec<ToolName>,
        /// How.
        via: SmolStr,
    },
    /// The run cannot continue without something from outside it.
    NeedsInput(NeedsInput),
    /// A JSON Patch against the shared state (AG-UI).
    StateDelta {
        /// RFC 6902 patch.
        patch: serde_json::Value,
    },
    /// A model call reported its usage.
    UsageUpdated {
        /// What that call consumed.
        usage: Usage,
    },
    /// A diagnostic worth surfacing.
    Warned {
        /// The diagnostic.
        warning: Warning,
    },
    /// The run ended.
    RunFinished {
        /// Everything it consumed.
        totals: UsageTotals,
        /// What it cost, when that can be said.
        cost: Option<CostEstimate>,
    },
}

/// An event, with everything needed to place it in a tree and a timeline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    /// Position in the journal. Monotonic per run, and the reason replay is deterministic.
    pub seq: SeqId,
    /// Which agent emitted it. Lets a UI render nested progress without the framework prescribing
    /// a layout.
    pub agent: AgentPath,
    /// What happened.
    pub kind: EventKind,
}

impl Event {
    /// An event from the root agent.
    #[must_use]
    pub fn root(seq: SeqId, kind: EventKind) -> Self {
        Self { seq, agent: AgentPath::root(), kind }
    }

    /// Whether this event may be dropped under backpressure.
    ///
    /// Only the two presentation deltas. Everything else changes the meaning of the transcript.
    #[must_use]
    pub fn is_droppable(&self) -> bool {
        self.kind.is_droppable()
    }
}

impl EventKind {
    /// Whether this event may be dropped under backpressure.
    #[must_use]
    pub fn is_droppable(&self) -> bool {
        matches!(self, Self::TextDelta { .. } | Self::ReasoningDelta { .. })
    }

    /// Whether this event must be written to the journal for replay to be faithful.
    ///
    /// Deltas are reconstructible from the final message, so they are journalled only when
    /// transcript fidelity is wanted.
    #[must_use]
    pub fn is_semantic(&self) -> bool {
        !self.is_droppable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ToolError, ToolErrorKind};

    fn all_kinds() -> Vec<EventKind> {
        vec![
            EventKind::RunStarted { run: RunId::new("r1") },
            EventKind::TurnStarted { turn: TurnId::FIRST },
            EventKind::TextDelta { text: "hi".into() },
            EventKind::ReasoningDelta { text: "hmm".into() },
            EventKind::ToolCallStarted {
                call: CallId::new("c1"),
                name: ToolName::new("fs_read"),
                args_preview: "{\"path\":\"a\"}".into(),
            },
            EventKind::ToolCallFinished { call: CallId::new("c1"), millis: 3, bytes_elided: 0 },
            EventKind::ToolCallFailed {
                call: CallId::new("c1"),
                error: ToolError::new(ToolErrorKind::NotFound, "gone"),
            },
            EventKind::Discovered { found: vec![ToolName::new("x")], via: "bm25".into() },
            EventKind::NeedsInput(NeedsInput { token: "t".into(), requests: Vec::new() }),
            EventKind::StateDelta { patch: serde_json::json!([]) },
            EventKind::UsageUpdated { usage: Usage::default() },
            EventKind::Warned { warning: Warning::BelowMinPrefix { have: 380, need: 512 } },
            EventKind::RunFinished { totals: UsageTotals::default(), cost: None },
        ]
    }

    #[test]
    fn exactly_two_event_kinds_are_droppable() {
        let droppable: Vec<_> = all_kinds().into_iter().filter(EventKind::is_droppable).collect();
        assert_eq!(
            droppable.len(),
            2,
            "only presentation deltas may be dropped; a new droppable variant needs a deliberate \
             decision, not a default"
        );
        assert!(
            droppable.iter().all(|k| matches!(
                k,
                EventKind::TextDelta { .. } | EventKind::ReasoningDelta { .. }
            ))
        );
    }

    #[test]
    fn semantic_and_droppable_partition_the_space() {
        for kind in all_kinds() {
            assert_ne!(kind.is_droppable(), kind.is_semantic(), "{kind:?}");
        }
    }

    #[test]
    fn events_carry_their_position_in_the_tree_and_the_journal() {
        let e = Event::root(SeqId(7), EventKind::TextDelta { text: "x".into() });
        assert!(e.agent.is_root());
        assert_eq!(e.seq, SeqId(7));
        assert!(e.is_droppable());
    }

    #[test]
    fn every_event_kind_round_trips() {
        for kind in all_kinds() {
            let event = Event::root(SeqId::FIRST, kind);
            let encoded = serde_json::to_string(&event).unwrap();
            let decoded: Event = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, event, "round trip failed for {encoded}");
        }
    }

    #[test]
    fn a_real_usage_blob_survives_the_event_envelope() {
        // The same tagging landmine as `Item`: `Usage` carries the provider's raw usage object, so
        // internal tagging on `EventKind` would corrupt it. `Usage::default()` has `raw: None` and
        // would not catch it, which is why this test uses a realistic payload.
        let raw = r#"{"cache_creation":{"ephemeral_5m_input_tokens":148,"ephemeral_1h_input_tokens":100}}"#;
        let usage = Usage {
            input: 50,
            cache_read: 100_000,
            cache_write: 5_120,
            raw: Some(serde_json::value::RawValue::from_string(raw.to_string()).unwrap()),
            ..Usage::default()
        };
        let event = Event::root(SeqId::FIRST, EventKind::UsageUpdated { usage });
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains(r#""usage_updated""#), "externally tagged; got {encoded}");

        let decoded: Event = serde_json::from_str(&encoded).unwrap();
        let EventKind::UsageUpdated { usage } = &decoded.kind else { panic!("wrong variant") };
        assert_eq!(usage.raw.as_ref().unwrap().get(), raw);
        assert_eq!(usage.total_input(), 105_170);
    }

    #[test]
    fn warnings_carry_enough_to_act_on() {
        let w = Warning::CacheChurn {
            segment: "system:prompts/system.md".into(),
            tokens: 12_400,
            advice: "a timestamp in the system prompt changes the prefix hash every turn".into(),
        };
        let rendered = serde_json::to_string(&w).unwrap();
        assert!(rendered.contains("12400"), "a warning without a number is not actionable");
        assert!(rendered.contains("system.md"), "a warning must name the culprit");
    }
}
