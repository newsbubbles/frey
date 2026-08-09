//! The A2A task lifecycle.
//!
//! Eight states, three of them terminal and two of them *interrupted*. The interrupted pair is the
//! interesting part: `INPUT_REQUIRED` and `AUTH_REQUIRED` mean the task is alive and waiting for
//! something from outside, which is the same concept as MCP's multi round-trip result and AG-UI's
//! interrupt. Frey therefore maps all three onto one [`NeedsInput`].
//!
//! The streaming rules below are quoted from the specification because they are the ones an
//! implementation gets wrong: a stream must terminate on a terminal state, events must reach every
//! subscriber in order, and closing one subscription must not disturb another.

use frey_core::error::{InputRequest, NeedsInput, Risk};
use frey_core::ids::{SessionId, TurnId};
use frey_core::taint::{Provenance, Tainted, Untrusted};
use smol_str::SmolStr;

/// Where a task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TaskState {
    /// Acknowledged and queued.
    TaskStateSubmitted,
    /// Being worked on.
    TaskStateWorking,
    /// Waiting for clarification. **Not terminal.**
    TaskStateInputRequired,
    /// Waiting for authentication. **Not terminal.**
    TaskStateAuthRequired,
    /// Finished successfully.
    TaskStateCompleted,
    /// Finished with an error.
    TaskStateFailed,
    /// Cancelled by the caller.
    TaskStateCanceled,
    /// Declined by the agent.
    TaskStateRejected,
}

impl TaskState {
    /// Whether the task is finished, one way or another.
    ///
    /// The specification requires a stream to terminate once a terminal state is observed, so this
    /// is the predicate the streaming layer is built on.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::TaskStateCompleted
                | Self::TaskStateFailed
                | Self::TaskStateCanceled
                | Self::TaskStateRejected
        )
    }

    /// Whether the task is alive and waiting for something from outside it.
    #[must_use]
    pub fn is_interrupted(self) -> bool {
        matches!(self, Self::TaskStateInputRequired | Self::TaskStateAuthRequired)
    }

    /// Whether work is still in progress.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::TaskStateSubmitted | Self::TaskStateWorking)
    }
}

/// A piece of a message or artefact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Part {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// A reference to something remote.
    Url {
        /// Where it is.
        url: String,
    },
    /// Inline bytes, base64-encoded.
    Raw {
        /// The bytes.
        raw: String,
    },
    /// Arbitrary structured data.
    Data {
        /// The value.
        data: serde_json::Value,
    },
}

/// Where a task is, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskStatus {
    /// The state.
    pub state: TaskState,
    /// An explanation, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When it changed.
    pub timestamp: SmolStr,
}

/// One unit of work handed to a peer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Task {
    /// Server-generated identifier.
    pub id: SmolStr,
    /// Groups related interactions. Maps to a Frey session.
    #[serde(rename = "contextId")]
    pub context_id: SmolStr,
    /// Where it is.
    pub status: TaskStatus,
    /// What it produced.
    #[serde(default)]
    pub artifacts: Vec<Part>,
}

impl Task {
    /// What a peer produced, labelled.
    ///
    /// Low integrity, always. A peer's artefacts are model output wearing a task envelope, and a
    /// signed agent card proves who produced them rather than whether they are true.
    #[must_use]
    pub fn artifact_text(&self) -> Untrusted<String> {
        let text = self
            .artifacts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Tainted::with_provenance(text, Provenance::new(format!("peer:{}", self.id)))
    }

    /// The session this task belongs to.
    #[must_use]
    pub fn session(&self) -> SessionId {
        SessionId::new(self.context_id.clone())
    }
}

/// Project an interrupted task onto Frey's one input-needed type.
///
/// This is the function ADR-0010 exists for. If it were awkward to write, the unification would be
/// wrong — and it would have been discovered far too late had A2A been added after the loop was
/// built around a different shape.
#[must_use]
pub fn needs_input(task: &Task) -> Option<NeedsInput> {
    if !task.status.state.is_interrupted() {
        return None;
    }
    let request = match task.status.state {
        TaskState::TaskStateAuthRequired => InputRequest::Auth {
            resource: task.status.message.clone().unwrap_or_else(|| task.id.to_string()),
        },
        _ => InputRequest::Choice {
            prompt: task
                .status
                .message
                .clone()
                .unwrap_or_else(|| "the peer needs clarification".into()),
            options: Vec::new(),
        },
    };
    Some(NeedsInput { token: format!("a2a:{}", task.id).into(), requests: vec![request] })
}

/// Whether an approval is required before handing work to a peer.
///
/// Always, for anything above low risk. A peer is a third party, and the prompt shows the literal
/// task text rather than a summary of it.
#[must_use]
pub fn delegation_approval(peer: &str, task_text: &str, risk: Risk) -> Option<NeedsInput> {
    if risk == Risk::Low {
        return None;
    }
    Some(NeedsInput {
        token: format!("delegate:{peer}").into(),
        requests: vec![InputRequest::Approval {
            literal: format!("send to `{peer}`: {task_text}"),
            risk,
        }],
    })
}

/// A stream of task updates, enforcing the specification's rules.
///
/// Those rules exist because implementations get them wrong: a stream must terminate once a
/// terminal state is observed, and closing one subscriber must not disturb another.
#[derive(Debug, Clone, Default)]
pub struct TaskStream {
    events: Vec<TaskStatus>,
    closed: bool,
}

impl TaskStream {
    /// A new, open stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push an update. Returns whether the stream is now closed.
    ///
    /// Updates after a terminal state are ignored rather than appended, because a subscriber that
    /// sees activity after completion cannot tell a late event from a new task.
    pub fn push(&mut self, status: TaskStatus) -> bool {
        if self.closed {
            return true;
        }
        let terminal = status.state.is_terminal();
        self.events.push(status);
        if terminal {
            self.closed = true;
        }
        self.closed
    }

    /// Everything pushed, in order.
    #[must_use]
    pub fn events(&self) -> &[TaskStatus] {
        &self.events
    }

    /// Whether the stream has terminated.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// What a subscriber joining now would see, from `after`.
    ///
    /// Resubscription replays from a position rather than from the start, which is what makes
    /// reconnecting after a dropped connection cheap.
    #[must_use]
    pub fn replay_from(&self, after: TurnId) -> &[TaskStatus] {
        let start = (after.index() as usize).min(self.events.len());
        &self.events[start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: TaskState) -> TaskStatus {
        TaskStatus { state, message: None, timestamp: "2026-08-09T00:00:00Z".into() }
    }

    fn task(state: TaskState) -> Task {
        Task {
            id: "task-1".into(),
            context_id: "session-7".into(),
            status: status(state),
            artifacts: vec![Part::Text { text: "the peer's answer".into() }],
        }
    }

    #[test]
    fn all_eight_states_classify_into_exactly_one_bucket() {
        let all = [
            TaskState::TaskStateSubmitted,
            TaskState::TaskStateWorking,
            TaskState::TaskStateInputRequired,
            TaskState::TaskStateAuthRequired,
            TaskState::TaskStateCompleted,
            TaskState::TaskStateFailed,
            TaskState::TaskStateCanceled,
            TaskState::TaskStateRejected,
        ];
        for state in all {
            let buckets = u8::from(state.is_terminal())
                + u8::from(state.is_interrupted())
                + u8::from(state.is_active());
            assert_eq!(buckets, 1, "{state:?} is in {buckets} buckets");
        }
        assert_eq!(all.iter().filter(|s| s.is_terminal()).count(), 4);
        assert_eq!(all.iter().filter(|s| s.is_interrupted()).count(), 2);
    }

    #[test]
    fn an_interrupted_task_projects_onto_the_one_needs_input_type() {
        // ADR-0010's falsifying test. If this were awkward, the unification across MCP, A2A and
        // AG-UI would be wrong.
        let mut waiting = task(TaskState::TaskStateInputRequired);
        waiting.status.message = Some("which repository?".into());

        let needs = needs_input(&waiting).expect("an interrupted task needs input");
        assert_eq!(needs.token.as_str(), "a2a:task-1");
        let InputRequest::Choice { prompt, .. } = &needs.requests[0] else {
            panic!("expected a choice")
        };
        assert_eq!(prompt, "which repository?");
    }

    #[test]
    fn an_auth_challenge_becomes_an_auth_request_rather_than_a_question() {
        let mut waiting = task(TaskState::TaskStateAuthRequired);
        waiting.status.message = Some("https://peer.test/oauth".into());
        let needs = needs_input(&waiting).unwrap();
        assert!(matches!(needs.requests[0], InputRequest::Auth { .. }));
    }

    #[test]
    fn a_running_or_finished_task_needs_nothing() {
        assert!(needs_input(&task(TaskState::TaskStateWorking)).is_none());
        assert!(needs_input(&task(TaskState::TaskStateCompleted)).is_none());
    }

    #[test]
    fn a_peers_output_is_low_integrity_whatever_its_card_says() {
        // TLS proves who said it, not whether it is true.
        let done = task(TaskState::TaskStateCompleted);
        let artifact = done.artifact_text();
        assert_eq!(artifact.peek(), "the peer's answer");
        assert_eq!(artifact.label().0, frey_core::taint::IntegrityLevel::Low);
        assert_eq!(artifact.provenance().origin.as_str(), "peer:task-1");
    }

    #[test]
    fn a_context_id_is_a_frey_session() {
        assert_eq!(task(TaskState::TaskStateWorking).session(), SessionId::new("session-7"));
    }

    #[test]
    fn a_stream_terminates_when_the_task_does_and_ignores_anything_after() {
        // A subscriber that sees activity after completion cannot tell a late event from a new task.
        let mut stream = TaskStream::new();
        assert!(!stream.push(status(TaskState::TaskStateWorking)));
        assert!(stream.push(status(TaskState::TaskStateCompleted)));
        assert!(stream.is_closed());

        stream.push(status(TaskState::TaskStateWorking));
        assert_eq!(stream.events().len(), 2, "nothing is appended after termination");
    }

    #[test]
    fn an_interrupted_state_does_not_close_the_stream() {
        // The whole reason those two states exist: the task is alive and waiting.
        let mut stream = TaskStream::new();
        assert!(!stream.push(status(TaskState::TaskStateInputRequired)));
        assert!(!stream.is_closed());
        assert!(!stream.push(status(TaskState::TaskStateWorking)));
    }

    #[test]
    fn resubscribing_replays_from_a_position_rather_than_the_start() {
        let mut stream = TaskStream::new();
        stream.push(status(TaskState::TaskStateSubmitted));
        stream.push(status(TaskState::TaskStateWorking));
        stream.push(status(TaskState::TaskStateCompleted));

        assert_eq!(stream.replay_from(TurnId(1)).len(), 2);
        assert_eq!(stream.replay_from(TurnId(3)).len(), 0);
        assert_eq!(
            stream.replay_from(TurnId(99)).len(),
            0,
            "an over-large position is not a panic"
        );
    }

    #[test]
    fn delegating_anything_above_low_risk_asks_first_and_shows_the_literal_task() {
        assert!(delegation_approval("peer", "summarise this", Risk::Low).is_none());

        let needs = delegation_approval("planner", "delete the staging database", Risk::High)
            .expect("high risk delegation is gated");
        let InputRequest::Approval { literal, .. } = &needs.requests[0] else {
            panic!("expected approval")
        };
        assert!(literal.contains("delete the staging database"), "no summarising: {literal}");
        assert!(literal.contains("planner"));
    }

    #[test]
    fn states_round_trip_in_the_wire_spelling() {
        let json = serde_json::to_string(&TaskState::TaskStateInputRequired).unwrap();
        assert_eq!(json, "\"TASK_STATE_INPUT_REQUIRED\"");
        assert_eq!(
            serde_json::from_str::<TaskState>(&json).unwrap(),
            TaskState::TaskStateInputRequired
        );
    }
}
