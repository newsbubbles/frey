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

use frey_context::hash::hash_parts;
use frey_core::event::Event;
use frey_core::ids::{RunId, SeqId};
use frey_core::item::Item;
use frey_core::provider::{Response, StopReason};
use frey_core::segment::ContentHash;
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
    /// A hash of what was actually *in* the prompt.
    ///
    /// **This was missing, and its absence made the headline replay claim narrower than it read.**
    /// The fingerprint was `{model, turns, tools}` — entirely shape. Change the system prompt, keep
    /// the same number of turns and the same tools, and a journal replayed **green**: divergence
    /// detection caught a different-looking run and not a different run.
    ///
    /// `Option` rather than a plain hash so journals written before this existed still load. `None`
    /// means *this record predates content hashing*, and it is compared as "unknown" rather than as
    /// "matches" — see [`RequestFingerprint::diverges_from`], which says which of the two it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentHash>,
}

/// How two fingerprints differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// Identical, content included.
    None,
    /// The shape matches and the content differs: same model, same turn count, same tools, and a
    /// different prompt.
    Content,
    /// Model, turn count or tool list differs.
    Shape,
    /// The shape matches and one side has no content hash, so the prompts cannot be compared.
    ///
    /// Reported rather than treated as a match. A journal recorded before content hashing existed
    /// can only be replayed for shape, and a replay that cannot see a difference should say so
    /// instead of reporting success it has not established.
    Unknown,
}

impl RequestFingerprint {
    /// Compare against what this run is asking for.
    #[must_use]
    pub fn diverges_from(&self, other: &Self) -> Divergence {
        if self.model != other.model || self.turns != other.turns || self.tools != other.tools {
            return Divergence::Shape;
        }
        match (self.content, other.content) {
            (Some(a), Some(b)) if a == b => Divergence::None,
            (Some(_), Some(_)) => Divergence::Content,
            _ => Divergence::Unknown,
        }
    }
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
    /// Presentation events discarded by [`record_event`](Self::record_event).
    ///
    /// `#[serde(default)]` so journals written before this existed still load; a reloaded journal
    /// reports zero, which is the honest answer — the count was never written down.
    #[serde(default)]
    dropped: u64,
}

impl Journal {
    /// An empty journal for `run`.
    #[must_use]
    pub fn new(run: RunId) -> Self {
        Self { run, entries: Vec::new(), events: Vec::new(), dropped: 0 }
    }

    /// Append an effect, returning its sequence number.
    pub fn record(&mut self, effect: Effect) -> SeqId {
        let seq = SeqId(u32::try_from(self.entries.len()).unwrap_or(u32::MAX));
        self.entries.push(Entry { seq, effect });
        seq
    }

    /// Append an event worth keeping in the transcript.
    ///
    /// Presentation events — text deltas, reasoning deltas, state patches — are **dropped**, and
    /// the count is kept. A journal is a record of what happened, not a replay of how it looked
    /// arriving, and keeping every delta would make a long run's transcript mostly typing.
    ///
    /// Counting them is the part that was missing. `Warning::EventsDropped` existed to report this
    /// exact number and nothing had ever produced it, so the one place in the framework that
    /// deliberately discards data did it silently — in a project whose stated rule is that nothing
    /// degrades quietly.
    pub fn record_event(&mut self, event: Event) {
        if event.kind.is_semantic() {
            self.events.push(event);
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    /// How many presentation events were discarded on the way in.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
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

        match request.diverges_from(fingerprint) {
            Divergence::None => {}
            Divergence::Unknown => {
                // Shape matches and the prompts cannot be compared. Allowed to proceed, because
                // refusing would make every journal written before content hashing unreplayable —
                // but it is not a clean match and the caller is told which it got.
                tracing::warn!(
                    seq = entry.seq.0,
                    "replaying a journal with no content hash: only the shape of this request was                      checked"
                );
            }
            Divergence::Content | Divergence::Shape => {
                return Err(ReplayError::Diverged {
                    seq: entry.seq,
                    recorded: describe(request),
                    actual: describe(fingerprint),
                });
            }
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
        "model `{}` with {} turn(s), tools [{}], content {}",
        fingerprint.model,
        fingerprint.turns,
        fingerprint.tools.join(", "),
        fingerprint.content.map_or_else(|| "not hashed".to_string(), |h| format!("{h:?}"))
    )
}

/// Hash what the prompt actually says, not how it is shaped.
///
/// Covers the text a dialect will encode and the tool definitions in full, since a description
/// changing under an unchanged tool *name* is exactly the substitution a shape-only fingerprint
/// waves through.
fn content_hash(request: &frey_core::provider::Request) -> ContentHash {
    let mut parts: Vec<String> = Vec::with_capacity(request.turns.len() + request.tools.len());
    for tool in &request.tools {
        parts.push(format!("{}|{}|{}", tool.name, tool.description, tool.input_schema.as_value()));
    }
    for turn in &request.turns {
        let mut rendered = format!("{:?}:", turn.role);
        for item in &turn.items {
            match item {
                Item::Text(text) => rendered.push_str(&text.text),
                Item::ToolCall(call) => {
                    rendered.push_str(call.name.as_str());
                    rendered.push_str(&call.args.to_string());
                }
                Item::ToolResult(result) => rendered.push_str(&result.content),
                _ => {}
            }
        }
        parts.push(rendered);
    }
    hash_parts(parts.iter().map(String::as_str))
}

/// Build a fingerprint from a request.
#[must_use]
pub fn fingerprint(request: &frey_core::provider::Request) -> RequestFingerprint {
    RequestFingerprint {
        model: request.model.as_str().into(),
        turns: u32::try_from(request.turns.len()).unwrap_or(u32::MAX),
        tools: request.tools.iter().map(|t| SmolStr::new(t.name.as_str())).collect(),
        content: Some(content_hash(request)),
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
mod replay_tests {
    use super::*;
    use frey_core::item::Turn;
    use frey_core::provider::Request;

    fn request(system: &str) -> Request {
        Request {
            model: frey_core::ids::ModelId::new("m"),
            turns: vec![Turn::system(system), Turn::user("go")],
            ..Request::default()
        }
    }

    #[test]
    fn a_changed_system_prompt_is_a_divergence_and_not_a_match() {
        // The finding this test exists to pin. `RequestFingerprint` was `{model, turns, tools}` —
        // pure shape — so a journal recorded against one persona replayed **green** against a
        // different one. Divergence detection caught a different-looking run, not a different run,
        // and the README claimed the second.
        let recorded = fingerprint(&request("you are a careful assistant"));
        let asking = fingerprint(&request("you are a reckless assistant"));

        assert_eq!(
            recorded.turns, asking.turns,
            "the shape is identical, which was the whole problem"
        );
        assert_eq!(recorded.tools, asking.tools);
        assert_eq!(recorded.diverges_from(&asking), Divergence::Content);
    }

    #[test]
    fn an_identical_request_does_not_diverge() {
        let a = fingerprint(&request("same"));
        assert_eq!(a.diverges_from(&fingerprint(&request("same"))), Divergence::None);
    }

    #[test]
    fn a_journal_written_before_content_hashing_reports_unknown_rather_than_a_match() {
        // The honest answer for an old recording: the prompts were not compared. Calling that a
        // match would be the same quiet degradation the content hash was added to remove.
        let mut old = fingerprint(&request("anything"));
        old.content = None;
        assert_eq!(
            old.diverges_from(&fingerprint(&request("something else"))),
            Divergence::Unknown
        );
    }

    #[test]
    fn a_recorded_run_replays_through_the_ordinary_agent_loop() {
        // Replay is reachable from the loop, which it was not: `next_response` had no caller
        // outside this file and `Agent::run` never mentioned it.
        use frey_core::provider::ModelProvider as _;

        let mut journal = Journal::new(RunId::new("r"));
        let req = request("you are a careful assistant");
        journal.record(Effect::ModelResponse {
            request: fingerprint(&req),
            items: vec![Item::text("the recorded answer")],
            usage: Usage::default(),
            stop: StopReason::EndTurn,
        });

        let replaying = Replaying::new(
            journal,
            frey_core::provider_caps::ProviderCapabilities::minimal(1_000, 100),
        );
        let response = pollster::block_on(replaying.complete(req)).expect("replays");
        assert_eq!(response.items, vec![Item::text("the recorded answer")]);
        assert_eq!(
            replaying.remaining(),
            0,
            "nothing left over means the whole run was reproduced"
        );
    }

    #[test]
    fn a_divergence_is_fatal_rather_than_retried() {
        use frey_core::provider::ModelProvider as _;

        let mut journal = Journal::new(RunId::new("r"));
        journal.record(Effect::ModelResponse {
            request: fingerprint(&request("recorded")),
            items: vec![Item::text("x")],
            usage: Usage::default(),
            stop: StopReason::EndTurn,
        });
        let replaying = Replaying::new(
            journal,
            frey_core::provider_caps::ProviderCapabilities::minimal(1_000, 100),
        );

        let error = pollster::block_on(replaying.complete(request("different"))).unwrap_err();
        assert!(
            format!("{error}").contains("diverged"),
            "the error must say what happened, not just that something did: {error}"
        );
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
        // **This test used to assert the opposite of its own name.** The fingerprint was shape only
        // — model, turn count, tool names — so the same turn moved from `User` to `System` matched,
        // and the test pinned that as "a deliberate limit". It was a limit; it was also the whole
        // difference between "replay reproduces a run" and "replay reproduces a run that looks like
        // this one", and the README claimed the first.
        let mut replay = Replay::new(recorded());
        let mut different = request(1, &["fs_read"]);
        different.turns = vec![Turn::new(Role::System, [Item::text("turn 0")])];
        assert!(replay.next_response(&fingerprint(&different)).is_err());
    }
}

/// A [`ModelProvider`] that answers from a recorded journal instead of a network.
///
/// **This is what made replay reachable.** `Replay::next_response` had zero callers outside this
/// file and `Agent::run` never mentioned it, so the capability was real, tested, and reachable only
/// by a caller willing to write their own loop — which is every caller, since the loop is the
/// product. Recording was wired; replaying was not.
///
/// Now it is an ordinary provider:
///
/// ```no_run
/// # use frey_agent::journal::{Journal, Replaying};
/// # use frey_agent::run::{Agent, ToolHost};
/// # fn demo<T: ToolHost>(journal: Journal, tools: T, caps: frey_core::provider_caps::ProviderCapabilities) {
/// let agent = Agent::new(Replaying::new(journal, caps), tools, "anthropic:claude-opus-5");
/// // `agent.run(task)` now consumes the recording, and refuses at the first divergence.
/// # }
/// ```
///
/// Tools **run for real**. That is deliberate and it is the sharp edge: a journal replayed against a
/// toolset that writes to a database writes to the database again. Replay reproduces the *model*,
/// which is the non-deterministic and expensive half; supply a read-only toolset if the other half
/// has side effects you do not want twice.
#[derive(Debug)]
pub struct Replaying {
    inner: std::sync::Mutex<Replay>,
    caps: frey_core::provider_caps::ProviderCapabilities,
}

impl Replaying {
    /// Replay `journal`, presenting `caps` so the budgeter and cache planner behave as they did.
    ///
    /// The capabilities are supplied rather than recorded because a journal does not hold them, and
    /// guessing them would silently change how the prompt was fitted — which would then look like a
    /// divergence in the run rather than in the harness.
    #[must_use]
    pub fn new(journal: Journal, caps: frey_core::provider_caps::ProviderCapabilities) -> Self {
        Self { inner: std::sync::Mutex::new(Replay::new(journal)), caps }
    }

    /// How many recorded responses have not been consumed.
    ///
    /// A replay that ends with responses left over reproduced a *prefix* of the run, which is not
    /// the same as reproducing it.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.inner.lock().expect("replay poisoned").remaining()
    }
}

impl frey_core::provider::ModelProvider for Replaying {
    fn id(&self) -> frey_core::ids::ProviderId {
        frey_core::ids::ProviderId::new("replay")
    }

    fn capabilities(
        &self,
        _model: &frey_core::ids::ModelId,
    ) -> frey_core::provider_caps::ProviderCapabilities {
        self.caps.clone()
    }

    fn complete(
        &self,
        request: frey_core::provider::Request,
    ) -> impl Future<Output = Result<Response, frey_core::provider::ProviderError>> + Send {
        let model = request.model.clone();
        let result = self
            .inner
            .lock()
            .expect("replay poisoned")
            .next_response(&fingerprint(&request))
            .map(|(items, usage, stop)| Response {
                items,
                usage,
                stop,
                model,
                provider: frey_core::ids::ProviderId::new("replay"),
            })
            // A protocol error, which the loop returns rather than retrying: a divergence retried
            // is a divergence absorbed, and a replay that improvises produces confident results
            // about a run that never happened.
            //
            // Deliberately *not* classified `is_fatal` — that flag means "your key or your billing
            // is broken, stop the whole program", and a divergence is a fact about this journal.
            .map_err(|error| frey_core::provider::ProviderError::Protocol {
                provider: frey_core::ids::ProviderId::new("replay"),
                detail: error.to_string(),
            });
        std::future::ready(result)
    }

    /// Refused rather than synthesised.
    ///
    /// A journal records what a response *was*, not the shape it arrived in. Replaying it as a
    /// stream would mean inventing a delta sequence that never happened and handing it to a
    /// consumer that cannot tell the difference — a replay improvising, which is the one thing this
    /// whole subsystem exists to refuse.
    fn stream(
        &self,
        _request: frey_core::provider::Request,
    ) -> impl Future<
        Output = Result<frey_core::provider::EventStream, frey_core::provider::ProviderError>,
    > + Send {
        std::future::ready(Err(frey_core::provider::ProviderError::Unsupported {
            provider: frey_core::ids::ProviderId::new("replay"),
            capability: "streaming a recorded run".into(),
        }))
    }
}

use std::future::Future;
