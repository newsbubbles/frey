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
    /// A turn ended, with where its wall-clock went.
    TurnFinished {
        /// Which turn.
        turn: TurnId,
        /// The breakdown.
        timing: TurnTiming,
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

/// Where one turn's wall-clock went, in microseconds.
///
/// **The point of this type is the line it draws.** Three of these phases are Frey's own work and
/// three are somebody else's, and a framework reporting one undivided number is reporting mostly
/// the network. The same mistake was made about MCP startup and measured away: 99.5% of what
/// looked like protocol cost was a process starting.
///
/// So [`Self::overhead_us`] is the number that means *"what did the framework cost"* — everything
/// except waiting for the provider and running the caller's tools. It is the only figure here worth
/// putting in a comparison against another framework, and it is deliberately the smallest one.
///
/// Microseconds because the framework phases are expected to be tens to hundreds of them and
/// milliseconds would round the interesting part to zero. `u64` of microseconds overflows after
/// about 584,000 years.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnTiming {
    /// Building segments from the tool definitions and the turn history.
    pub segment_us: u64,
    /// [`crate::segment::Segment`] budgeting — deciding what to evict.
    pub budget_us: u64,
    /// Cache planning: breakpoint placement, churn and minimum-prefix checks.
    pub plan_us: u64,
    /// Applying the eviction and building the request, including cloning it.
    pub assemble_us: u64,
    /// **Not Frey.** Waiting for the provider: network, queue, and inference.
    pub provider_us: u64,
    /// Decoding the response, reconciling the estimate, accounting, and emitting events.
    pub account_us: u64,
    /// **Not Frey.** Running the caller's tools.
    pub tools_us: u64,
    /// The whole turn, wall-clock.
    pub total_us: u64,
}

impl TurnTiming {
    /// What the framework itself cost: everything but the provider wait and the caller's tools.
    ///
    /// Computed by subtraction rather than by summing the phases, so anything happening in the loop
    /// that nobody thought to instrument lands **inside** the overhead figure instead of vanishing.
    /// A breakdown that always adds up is a breakdown that cannot show you a surprise.
    #[must_use]
    pub fn overhead_us(&self) -> u64 {
        self.total_us.saturating_sub(self.provider_us).saturating_sub(self.tools_us)
    }

    /// The share of the turn Frey is responsible for, in **parts per million**.
    ///
    /// **This was per-mille until the first measurement against a live provider**, where it read
    /// `0` on every row of every level of both models — 30 µs of framework against 800 ms of
    /// network rounds to nothing at one part in a thousand. A column of zeros is not a result, it
    /// is a unit that cannot express the result, and the fake-provider sweep could never have shown
    /// that because its latency was a constant somebody chose.
    ///
    /// Parts per million puts a realistic turn in the tens: ~45 ppm at 32 µs against 700 ms. Still
    /// an integer, because this goes in a journal that gets diffed.
    #[must_use]
    pub fn overhead_ppm(&self) -> u64 {
        if self.total_us == 0 {
            return 0;
        }
        self.overhead_us().saturating_mul(1_000_000) / self.total_us
    }

    /// The phases Frey instruments, summed — which is **not** [`Self::overhead_us`].
    ///
    /// The gap between the two is uninstrumented loop time. Watching it grow is the point.
    #[must_use]
    pub fn accounted_us(&self) -> u64 {
        self.segment_us
            .saturating_add(self.budget_us)
            .saturating_add(self.plan_us)
            .saturating_add(self.assemble_us)
            .saturating_add(self.account_us)
    }
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
            EventKind::TurnFinished {
                turn: TurnId(0),
                timing: TurnTiming { total_us: 1_000, provider_us: 900, ..TurnTiming::default() },
            },
            EventKind::RunFinished { totals: UsageTotals::default(), cost: None },
        ]
    }

    /// The whole reason this type is not one number.
    #[test]
    fn overhead_excludes_the_provider_and_the_callers_tools() {
        let t = TurnTiming {
            segment_us: 40,
            budget_us: 60,
            plan_us: 80,
            assemble_us: 120,
            provider_us: 2_000_000,
            account_us: 100,
            tools_us: 500_000,
            total_us: 2_500_400,
        };
        assert_eq!(t.overhead_us(), 400, "the framework's share, not the turn's wall-clock");
        // 159 ppm — 0.016% of the turn, truncated rather than rounded, which is the right
        // direction for a figure that will be quoted. In per-mille this read `0`, and a column of
        // zeros against a live provider is what sent the unit back for a rewrite.
        assert_eq!(t.overhead_ppm(), 159);
        assert_eq!(t.accounted_us(), 400, "every microsecond is attributed to a named phase");
    }

    /// Overhead is `total - provider - tools`, never the sum of the phases.
    ///
    /// If it were the sum, time spent in the loop that nobody instrumented would vanish from the
    /// report — the measurement would be defined as complete rather than measured to be, which is
    /// the shape of every defect in `notes/INCIDENTS.md`.
    #[test]
    fn time_nobody_instrumented_shows_up_rather_than_disappearing() {
        let t = TurnTiming {
            segment_us: 10,
            budget_us: 10,
            plan_us: 10,
            assemble_us: 10,
            account_us: 10,
            provider_us: 1_000,
            tools_us: 0,
            total_us: 1_950, // 900 µs somewhere nobody put a clock
        };
        assert_eq!(t.accounted_us(), 50);
        assert_eq!(t.overhead_us(), 950);
        assert_eq!(
            t.overhead_us() - t.accounted_us(),
            900,
            "uninstrumented loop time has to be visible, not defined away"
        );
    }

    #[test]
    fn a_turn_that_took_no_time_does_not_divide_by_zero() {
        assert_eq!(TurnTiming::default().overhead_ppm(), 0);
    }

    #[test]
    fn a_timing_survives_the_journal() {
        let t = TurnTiming { segment_us: 1, provider_us: 2, total_us: 9, ..TurnTiming::default() };
        let round: TurnTiming =
            serde_json::from_str(&serde_json::to_string(&t).expect("encode")).expect("decode");
        assert_eq!(round, t);
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
