//! The run journal, and replay.
//!
//! **The journal is the session.** Resuming replays it; there is no separate state that can drift
//! from the transcript. That single decision buys four things that are usually built separately:
//! regression tests over real transcripts, crash resumption, cheap prompt A/B on recorded runs, and
//! a debugging tool that reproduces a failure exactly.
//!
//! It works because everything non-deterministic is recorded: model responses, tool results,
//! discovery outcomes. Replay feeds those back and **diverges loudly at the first mismatch**, naming
//! the step — silent divergence would make replay worse than useless, since it would produce
//! confident results about a run that never happened.

use frey_core::event::Event;
use frey_core::ids::{RunId, SeqId};
use frey_core::item::Item;
use frey_core::provider::{Response, StopReason};
use frey_core::usage::{Usage, UsageTotals};
use smol_str::SmolStr;

/// One recorded non-deterministic effect.
///
/// Deterministic work is deliberately absent: prompt assembly, cache planning and budgeting are
/// pure functions of the recorded inputs, so recording their outputs would let a real change hide
/// behind a stale recording.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Effect {
    /// A model produced a response.
    ModelResponse {
        /// A fingerprint of the request that produced it, so replay can detect divergence.
        request: RequestFingerprint,
        /// What came back.
        items: Vec<Item>,
        /// What it consumed.
        usage: Usage,
        /// Why it stopped.
        stop: StopReason,
    },
    /// A tool produced a result.
    ToolResult {
        /// Which tool.
        tool: SmolStr,
        /// What it returned, as the model saw it.
        content: String,
        /// Whether it failed.
        is_error: bool,
    },
    /// Something outside the run was needed and supplied.
    InputSupplied {
        /// What was asked for.
        request: SmolStr,
        /// What came back.
        response: SmolStr,
    },
}

impl Effect {
    /// A short label, so a journal can be read without a debugger.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::ModelResponse { .. } => "model-response",
            Self::ToolResult { .. } => "tool-result",
            Self::InputSupplied { .. } => "input-supplied",
        }
    }
}

/// Enough of a request to notice that it changed, without storing the whole prompt twice.
///
/// Turn count and tool names rather than full text: a journal that stored every prompt verbatim
/// would be enormous, and these are the parts that change when the *shape* of a run changes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestFingerprint {
    /// Which model.
    pub model: SmolStr,
    /// How many turns were sent.
    pub turns: u32,
    /// Which tools were visible, in presentation order.
    pub tools: Vec<SmolStr>,
}

/// One entry in the journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Position in the run. Monotonic, and the reason replay is deterministic.
    pub seq: SeqId,
    /// What happened.
    pub effect: Effect,
}

/// An append-only record of one run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Journal {
    /// Which run.
    pub run: RunId,
    /// What happened, in order.
    pub entries: Vec<Entry>,
    /// Everything the run emitted, for transcript rendering. Presentation deltas are not stored,
    /// because they are reconstructible from the final items and would dominate the file.
    #[serde(default)]
    pub events: Vec<Event>,
}

impl Journal {
    /// An empty journal for `run`.
    #[must_use]
    pub fn new(run: RunId) -> Self {
        Self { run, entries: Vec::new(), events: Vec::new() }
    }

    /// Append an effect, returning its sequence number.
    pub fn record(&mut self, effect: Effect) -> SeqId {
        let seq = SeqId(u32::try_from(self.entries.len()).unwrap_or(u32::MAX));
        self.entries.push(Entry { seq, effect });
        seq
    }

    /// Append an event worth keeping in the transcript.
    pub fn record_event(&mut self, event: Event) {
        if event.kind.is_semantic() {
            self.events.push(event);
        }
    }

    /// How many effects were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What the run consumed, recomputed from the record.
    ///
    /// The journal is the session, so this is derivable rather than something to carry alongside —
    /// and it is the only way to price a run that ended in a way that has no [`RunOutput`]. A
    /// looping agent still spent money, and a turn limit that reports no cost is how a runaway
    /// becomes invisible in the ledger.
    ///
    /// [`RunOutput`]: crate::run::RunOutput
    #[must_use]
    pub fn totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for entry in &self.entries {
            if let Effect::ModelResponse { usage, .. } = &entry.effect {
                // A mixed-currency run cannot be summed, and inventing a figure is worse than
                // reporting the tokens and no total.
                let _ = totals.record("run", usage);
            }
        }
        totals
    }

    /// Serialise as JSON Lines, so a journal can be appended to and inspected with ordinary tools.
    ///
    /// One record per line, effects first and then events. Effect lines are bare [`Entry`] objects
    /// and event lines are wrapped as `{"event": …}`, which is what lets a reader tell them apart
    /// in a stream and what keeps files written before events were persisted readable.
    ///
    /// **This used to write effects only.** Replay is defined over effects, so a journal that had
    /// been through a file still replayed perfectly and reported an empty transcript — a caller who
    /// persisted and reloaded got something that worked and said nothing, with no way to tell it
    /// apart from a run that genuinely emitted no events. That is the quiet degradation this
    /// project exists to refuse, and it was invisible because the round-trip test compared
    /// `entries` rather than the whole journal. It now compares the whole journal.
    ///
    /// # Errors
    /// Returns the serialisation failure.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        for event in &self.events {
            out.push_str(&serde_json::to_string(&serde_json::json!({ "event": event }))?);
            out.push('\n');
        }
        Ok(out)
    }

    /// Parse a journal from JSON Lines.
    ///
    /// Accepts both line shapes, so a file written by an earlier version — effects only, no
    /// wrapper — loads as a journal with an empty event stream rather than failing.
    ///
    /// # Errors
    /// Returns the first line that failed to parse.
    pub fn from_jsonl(run: RunId, text: &str) -> Result<Self, serde_json::Error> {
        let mut journal = Self::new(run);
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)?;
            match value.get("event") {
                Some(event) => journal.events.push(serde_json::from_value(event.clone())?),
                None => journal.entries.push(serde_json::from_value(value)?),
            }
        }
        Ok(journal)
    }
}

/// A recorded run diverged from what is being replayed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReplayError {
    /// The journal ran out before the run did.
    #[error(
        "the journal ended after {recorded} step(s) but the run wanted another. The run is longer \
         than the recording, so something upstream changed."
    )]
    Exhausted {
        /// How many steps were recorded.
        recorded: usize,
    },
    /// The run asked for something different from what was recorded.
    #[error(
        "replay diverged at step {seq}: recorded {recorded}, but this run produced {actual}. \
         Divergence is reported rather than absorbed, because a replay that quietly adapts is \
         worse than no replay at all."
    )]
    Diverged {
        /// Where.
        seq: SeqId,
        /// What the journal holds.
        recorded: String,
        /// What the run asked for.
        actual: String,
    },
}

/// Feeds a recorded journal back, in order, refusing to improvise.
#[derive(Debug, Clone)]
pub struct Replay {
    journal: Journal,
    next: usize,
}

impl Replay {
    /// Replay `journal` from the beginning.
    #[must_use]
    pub fn new(journal: Journal) -> Self {
        Self { journal, next: 0 }
    }

    /// Take the next recorded model response, checking that the request still matches.
    ///
    /// # Errors
    /// Returns [`ReplayError`] when the journal is exhausted or the request changed.
    pub fn next_response(
        &mut self,
        fingerprint: &RequestFingerprint,
    ) -> Result<(Vec<Item>, Usage, StopReason), ReplayError> {
        let entry = self
            .journal
            .entries
            .get(self.next)
            .ok_or(ReplayError::Exhausted { recorded: self.journal.entries.len() })?;

        let Effect::ModelResponse { request, items, usage, stop } = &entry.effect else {
            return Err(ReplayError::Diverged {
                seq: entry.seq,
                recorded: format!("{:?}", entry.effect),
                actual: "a model call".into(),
            });
        };

        if request != fingerprint {
            return Err(ReplayError::Diverged {
                seq: entry.seq,
                recorded: describe(request),
                actual: describe(fingerprint),
            });
        }

        self.next += 1;
        Ok((items.clone(), usage.clone(), stop.clone()))
    }

    /// How many effects remain unconsumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.journal.entries.len().saturating_sub(self.next)
    }

    /// The journal being replayed.
    #[must_use]
    pub fn journal(&self) -> &Journal {
        &self.journal
    }
}

fn describe(fingerprint: &RequestFingerprint) -> String {
    format!(
        "model `{}` with {} turn(s) and tools [{}]",
        fingerprint.model,
        fingerprint.turns,
        fingerprint.tools.join(", ")
    )
}

/// Build a fingerprint from a request.
#[must_use]
pub fn fingerprint(request: &frey_core::provider::Request) -> RequestFingerprint {
    RequestFingerprint {
        model: request.model.as_str().into(),
        turns: u32::try_from(request.turns.len()).unwrap_or(u32::MAX),
        tools: request.tools.iter().map(|t| SmolStr::new(t.name.as_str())).collect(),
    }
}

/// Turn a response into the effect that records it.
#[must_use]
pub fn effect_of(request: &frey_core::provider::Request, response: &Response) -> Effect {
    Effect::ModelResponse {
        request: fingerprint(request),
        items: response.items.clone(),
        usage: response.usage.clone(),
        stop: response.stop.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::ids::ModelId;
    use frey_core::item::{Role, Turn};
    use frey_core::provider::Request;
    use frey_core::tool_def::{JsonSchema, ToolDefinition};

    fn request(turns: usize, tools: &[&str]) -> Request {
        Request {
            model: ModelId::new("test-model"),
            turns: (0..turns).map(|i| Turn::user(format!("turn {i}"))).collect(),
            tools: tools
                .iter()
                .map(|t| ToolDefinition::new(*t, "A described tool", JsonSchema::empty_object()))
                .collect(),
            ..Request::default()
        }
    }

    fn response(text: &str) -> Response {
        Response {
            items: vec![Item::text(text)],
            usage: Usage::default(),
            stop: StopReason::EndTurn,
            model: ModelId::new("test-model"),
            provider: frey_core::ids::ProviderId::new("scripted"),
        }
    }

    fn recorded() -> Journal {
        let mut journal = Journal::new(RunId::new("r1"));
        journal.record(effect_of(&request(1, &["fs_read"]), &response("first")));
        journal.record(effect_of(&request(3, &["fs_read"]), &response("second")));
        journal
    }

    #[test]
    fn a_replayed_run_reproduces_the_recording_exactly() {
        let mut replay = Replay::new(recorded());
        let (items, _, stop) =
            replay.next_response(&fingerprint(&request(1, &["fs_read"]))).unwrap();
        assert_eq!(items, vec![Item::text("first")]);
        assert_eq!(stop, StopReason::EndTurn);

        let (items, _, _) = replay.next_response(&fingerprint(&request(3, &["fs_read"]))).unwrap();
        assert_eq!(items, vec![Item::text("second")]);
        assert_eq!(replay.remaining(), 0);
    }

    #[test]
    fn a_changed_prompt_diverges_at_the_exact_step_rather_than_adapting() {
        // The property that makes replay trustworthy. Absorbing the difference would produce
        // confident results about a run that never happened.
        let mut replay = Replay::new(recorded());
        replay.next_response(&fingerprint(&request(1, &["fs_read"]))).unwrap();

        let err = replay
            .next_response(&fingerprint(&request(3, &["fs_read", "shell"])))
            .expect_err("an extra tool changes the request");
        let ReplayError::Diverged { seq, recorded, actual } = &err else {
            panic!("expected divergence, got {err:?}")
        };
        assert_eq!(*seq, SeqId(1), "the exact step");
        assert!(recorded.contains("fs_read"), "{recorded}");
        assert!(actual.contains("shell"), "{actual}");
    }

    #[test]
    fn a_run_longer_than_its_recording_says_so() {
        let mut replay = Replay::new(recorded());
        replay.next_response(&fingerprint(&request(1, &["fs_read"]))).unwrap();
        replay.next_response(&fingerprint(&request(3, &["fs_read"]))).unwrap();

        let err = replay.next_response(&fingerprint(&request(5, &["fs_read"]))).unwrap_err();
        assert!(matches!(err, ReplayError::Exhausted { recorded: 2 }));
        assert!(format!("{err}").contains("longer than the recording"));
    }

    #[test]
    fn journals_round_trip_through_json_lines() {
        let journal = recorded();
        let text = journal.to_jsonl().unwrap();
        assert_eq!(text.lines().count(), 2, "one line per effect, so it can be tailed");

        let parsed = Journal::from_jsonl(RunId::new("r1"), &text).unwrap();
        assert_eq!(parsed.entries, journal.entries);
    }

    /// The whole journal, not just the effects it replays from.
    ///
    /// This assertion used to compare `entries` alone, which is why nobody noticed that a journal
    /// through a file lost its entire event stream. It replayed perfectly and reported nothing —
    /// indistinguishable from a run that emitted no events, which is the quiet degradation this
    /// project claims not to have.
    #[test]
    fn the_transcript_survives_a_round_trip_not_only_the_effects() {
        use frey_core::event::{Event, EventKind};
        use frey_core::ids::{CallId, ToolName};

        let mut journal = recorded();
        journal.record_event(Event::root(
            SeqId(0),
            EventKind::ToolCallStarted {
                call: CallId::new("c1"),
                name: ToolName::new("fs_read"),
                args_preview: "{\"path\":\"a\"}".into(),
            },
        ));
        journal.record_event(Event::root(
            SeqId(1),
            EventKind::TurnStarted { turn: frey_core::ids::TurnId(0) },
        ));
        assert!(!journal.events.is_empty(), "the fixture has something to lose");

        let parsed = Journal::from_jsonl(RunId::new("r1"), &journal.to_jsonl().unwrap()).unwrap();
        assert_eq!(parsed, journal, "the whole journal, not just what replay needs");
    }

    /// A file written before events were persisted must still load. deadnet had journals on disk
    /// within an hour of wiring Frey up, so "nobody has files yet" was already false.
    #[test]
    fn a_file_without_event_lines_still_loads() {
        let effects_only = recorded()
            .entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let parsed = Journal::from_jsonl(RunId::new("r1"), &effects_only).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert!(parsed.events.is_empty(), "an old file genuinely has none");
    }

    #[test]
    fn presentation_deltas_are_not_stored() {
        // Deltas are reconstructible from the final items and would otherwise dominate the file.
        use frey_core::event::{Event, EventKind};
        let mut journal = Journal::new(RunId::new("r1"));
        journal.record_event(Event::root(SeqId::FIRST, EventKind::TextDelta { text: "a".into() }));
        journal
            .record_event(Event::root(SeqId(1), EventKind::RunStarted { run: RunId::new("r1") }));
        assert_eq!(journal.events.len(), 1);
        assert!(matches!(journal.events[0].kind, EventKind::RunStarted { .. }));
    }

    #[test]
    fn effects_are_labelled_so_a_journal_reads_without_a_debugger() {
        let journal = recorded();
        assert_eq!(journal.entries[0].effect.label(), "model-response");
    }

    #[test]
    fn sequence_numbers_are_dense_and_ordered() {
        let journal = recorded();
        let seqs: Vec<u32> = journal.entries.iter().map(|e| e.seq.index()).collect();
        assert_eq!(seqs, vec![0, 1], "replay indexes by position, so gaps would break it");
    }

    #[test]
    fn a_turn_role_change_is_not_mistaken_for_the_same_request() {
        let mut replay = Replay::new(recorded());
        let mut different = request(1, &["fs_read"]);
        different.turns = vec![Turn::new(Role::System, [Item::text("turn 0")])];
        // Same turn count, so the fingerprint matches: this is a deliberate limit, documented on
        // `RequestFingerprint`, and it is why divergence checks shape rather than content.
        assert!(replay.next_response(&fingerprint(&different)).is_ok());
    }
}
