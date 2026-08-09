//! The AG-UI projection.
//!
//! There is no adapter here, only a serialiser. Frey's internal event bus was built in AG-UI's
//! shape (ADR-0015), so a frontend protocol falls out of the design rather than being bolted onto
//! it. The events below are a projection of [`frey_core::event::Event`], not a parallel model that
//! could drift from it.

use frey_core::event::{Event, EventKind};
use serde_json::{Value, json};

/// One AG-UI frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The event name a frontend switches on.
    pub name: &'static str,
    /// Its payload.
    pub data: Value,
}

impl Frame {
    /// Render as a server-sent event.
    #[must_use]
    pub fn to_sse(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.name, self.data)
    }
}

/// Project an internal event onto the wire.
///
/// Returns `None` for events a frontend has no use for, rather than inventing a frame — a stream
/// that carries frames nothing consumes is a stream people stop reading.
#[must_use]
pub fn project(event: &Event) -> Option<Frame> {
    let agent = event.agent.to_string();
    let frame = match &event.kind {
        EventKind::RunStarted { run } => {
            Frame { name: "RUN_STARTED", data: json!({"runId": run.as_str(), "agent": agent}) }
        }
        EventKind::TextDelta { text } => {
            Frame { name: "TEXT_MESSAGE_CONTENT", data: json!({"delta": text, "agent": agent}) }
        }
        EventKind::ReasoningDelta { text } => {
            Frame { name: "THINKING_CONTENT", data: json!({"delta": text, "agent": agent}) }
        }
        EventKind::ToolCallStarted { call, name, args_preview } => Frame {
            name: "TOOL_CALL_START",
            data: json!({
                "toolCallId": call.as_str(),
                "toolName": name.as_str(),
                "args": args_preview,
                "agent": agent,
            }),
        },
        EventKind::ToolCallFinished { call, millis, bytes_elided } => Frame {
            name: "TOOL_CALL_END",
            data: json!({
                "toolCallId": call.as_str(),
                "durationMs": millis,
                // Surfaced rather than hidden: a user reading a truncated result should be able to
                // see that it was truncated.
                "bytesElided": bytes_elided,
                "agent": agent,
            }),
        },
        EventKind::ToolCallFailed { call, error } => Frame {
            name: "TOOL_CALL_ERROR",
            data: json!({
                "toolCallId": call.as_str(),
                // The *user* rendering when there is one, otherwise the model's. Never the operator
                // diagnostic: a stack trace in a UI helps nobody and can leak a path or a hostname.
                "message": error
                    .user()
                    .map_or_else(|| error.model().summary.clone(), |p| p.message.clone()),
                "agent": agent,
            }),
        },
        EventKind::NeedsInput(needs) => Frame {
            name: "INTERRUPT",
            data: json!({
                "token": needs.token.as_str(),
                "requests": needs.requests.len(),
                "agent": agent,
            }),
        },
        EventKind::StateDelta { patch } => {
            Frame { name: "STATE_DELTA", data: json!({"patch": patch}) }
        }
        EventKind::Warned { warning } => {
            Frame { name: "WARNING", data: serde_json::to_value(warning).unwrap_or(Value::Null) }
        }
        EventKind::RunFinished { totals, .. } => Frame {
            name: "RUN_FINISHED",
            data: json!({
                "agent": agent,
                "unmeteredCalls": totals.unmetered_calls,
                // A cost the provider never reported is absent rather than zero, because a zero in
                // a UI reads as "this was free".
                "cost": totals.reported_cost.map(|c| c.micros),
            }),
        },
        // Turn boundaries and usage updates are internal bookkeeping; a frontend renders neither.
        _ => return None,
    };
    Some(frame)
}

/// Project a stream of events, honouring the drop rule.
///
/// Presentation frames may be dropped when a consumer cannot keep up; semantic ones may not. The
/// count of what went is returned so lag is diagnosable.
#[must_use]
pub fn project_stream(events: &[Event], capacity: usize) -> (Vec<Frame>, u64) {
    let mut frames = Vec::new();
    let mut dropped = 0u64;
    for event in events {
        let Some(frame) = project(event) else { continue };
        if frames.len() >= capacity && event.is_droppable() {
            dropped += 1;
            continue;
        }
        frames.push(frame);
    }
    (frames, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::error::{Diagnostic, ToolError, ToolErrorKind};
    use frey_core::ids::{CallId, RunId, SeqId, ToolName};

    #[test]
    fn every_internal_event_either_projects_or_is_deliberately_silent() {
        // The projection is total by construction. A frontend that receives frames nothing consumes
        // is a frontend whose author stops reading the stream.
        let projected =
            project(&Event::root(SeqId::FIRST, EventKind::RunStarted { run: RunId::new("r1") }));
        assert!(projected.is_some());

        let internal = project(&Event::root(
            SeqId::FIRST,
            EventKind::TurnStarted { turn: frey_core::ids::TurnId::FIRST },
        ));
        assert!(internal.is_none(), "turn boundaries are bookkeeping, not UI");
    }

    #[test]
    fn an_operator_diagnostic_never_reaches_the_frontend() {
        // A stack trace in a UI helps nobody, and it can leak a path or a hostname.
        let error = ToolError::new(ToolErrorKind::NotFound, "no such file")
            .diagnose(Diagnostic::new("ENOENT on /home/nate/secret-project/src/main.rs"));
        let frame = project(&Event::root(
            SeqId::FIRST,
            EventKind::ToolCallFailed { call: CallId::new("c1"), error },
        ))
        .unwrap();

        let rendered = frame.data.to_string();
        assert!(!rendered.contains("/home/nate"), "{rendered}");
        assert!(rendered.contains("no such file"));
    }

    #[test]
    fn a_user_facing_message_is_preferred_when_there_is_one() {
        let error = ToolError::new(ToolErrorKind::Denied, "capability missing")
            .present("That action needs permission you have not granted.");
        let frame = project(&Event::root(
            SeqId::FIRST,
            EventKind::ToolCallFailed { call: CallId::new("c1"), error },
        ))
        .unwrap();
        assert!(frame.data["message"].as_str().unwrap().contains("needs permission"));
    }

    #[test]
    fn truncation_is_visible_to_a_user_rather_than_hidden() {
        let frame = project(&Event::root(
            SeqId::FIRST,
            EventKind::ToolCallFinished { call: CallId::new("c1"), millis: 12, bytes_elided: 4096 },
        ))
        .unwrap();
        assert_eq!(frame.data["bytesElided"], json!(4096));
    }

    #[test]
    fn an_unreported_cost_is_absent_rather_than_zero() {
        // A zero in a UI reads as "this was free", which is a different claim from "nobody said".
        let frame = project(&Event::root(
            SeqId::FIRST,
            EventKind::RunFinished { totals: frey_core::usage::UsageTotals::default(), cost: None },
        ))
        .unwrap();
        assert_eq!(frame.data["cost"], Value::Null);
    }

    #[test]
    fn an_interrupt_is_its_own_frame_type() {
        // The same concept as MCP's input_required and A2A's interrupted states, projected once.
        let frame = project(&Event::root(
            SeqId::FIRST,
            EventKind::NeedsInput(frey_core::error::NeedsInput {
                token: "approve:c1".into(),
                requests: vec![frey_core::error::InputRequest::Approval {
                    literal: "git push --force".into(),
                    risk: frey_core::error::Risk::High,
                }],
            }),
        ))
        .unwrap();
        assert_eq!(frame.name, "INTERRUPT");
        assert_eq!(frame.data["token"], json!("approve:c1"));
    }

    #[test]
    fn frames_render_as_server_sent_events() {
        let frame = Frame { name: "TEXT_MESSAGE_CONTENT", data: json!({"delta": "hi"}) };
        let sse = frame.to_sse();
        assert!(sse.starts_with("event: TEXT_MESSAGE_CONTENT\n"));
        assert!(sse.ends_with("\n\n"), "a frame must terminate or the next one merges into it");
    }

    #[test]
    fn backpressure_drops_deltas_and_keeps_semantics() {
        let mut events: Vec<Event> = (0..20)
            .map(|i| Event::root(SeqId(i), EventKind::TextDelta { text: "x".into() }))
            .collect();
        events.push(Event::root(
            SeqId(20),
            EventKind::ToolCallStarted {
                call: CallId::new("c1"),
                name: ToolName::new("fs_read"),
                args_preview: "{}".into(),
            },
        ));

        let (frames, dropped) = project_stream(&events, 5);
        assert!(dropped > 0);
        assert!(
            frames.iter().any(|f| f.name == "TOOL_CALL_START"),
            "a semantic frame must survive whatever the pressure"
        );
    }

    #[test]
    fn the_agent_path_travels_with_every_frame_so_a_ui_can_nest() {
        let path =
            frey_core::ids::AgentPath::root().child(frey_core::ids::AgentId::new("researcher"));
        let event = Event {
            seq: SeqId::FIRST,
            agent: path,
            kind: EventKind::TextDelta { text: "hi".into() },
        };
        assert_eq!(project(&event).unwrap().data["agent"], json!("root/researcher"));
    }
}
