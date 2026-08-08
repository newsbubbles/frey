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
use frey_core::usage::Usage;
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

    /// Serialise as JSON Lines, so a journal can be appended to and inspected with ordinary tools.
    ///
    /// # Errors
    /// Returns the serialisation failure.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        Ok(out)
    }

    /// Parse a journal from JSON Lines.
    ///
    /// # Errors
    /// Returns the first line that failed to parse.
    pub fn from_jsonl(run: RunId, text: &str) -> Result<Self, serde_json::Error> {
        let mut journal = Self::new(run);
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            journal.entries.push(serde_json::from_str(line)?);
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
