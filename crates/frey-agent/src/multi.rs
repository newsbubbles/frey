//! Many agents.
//!
//! This module is mostly about saying no. Multi-agent is where frameworks acquire graph DSLs,
//! supervisor trees and swarm metaphors, and where claims become unfalsifiable. Frey ships four
//! primitives and one invariant, and the invariant is the part that matters.
//!
//! # The invariant: capabilities only narrow
//!
//! A child's grants are always a subset of its parent's. That is the structural defence against the
//! escalation the injection literature describes as growing multiplicatively with pipeline depth: a
//! compromised sub-agent can misbehave within its own grants and no further, however convincingly
//! its output argues otherwise.
//!
//! # Three kinds of "another agent", kept apart
//!
//! A sub-agent is Frey's own code. A delegated agent is a vendor process that owns its auth, tools
//! and sandbox. A peer is someone else's service. All three produce **low-integrity** output, and
//! there is deliberately no "trusted agent" tier — a peer's reply is model output wearing an
//! envelope, and TLS proves who said it, not whether it is true.

use frey_core::capability::{GrantSet, SessionPowers};
use frey_core::event::Event;
use frey_core::ids::{AgentId, AgentPath};
use frey_core::taint::{Provenance, Tainted, Untrusted};

/// What a child inherits from its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inheritance {
    /// Nothing. The child sees only its task.
    None,
    /// A written brief. The default, and the one that keeps a quarantined agent quarantined.
    Summary(String),
    /// The parent's full context, shared read-only.
    ///
    /// Shared through an `Arc` rather than copied or passed through a mailbox: agents mostly want
    /// read-mostly context, and making them serialise it is the cost the actor model imposes for a
    /// guarantee they do not need.
    Snapshot(std::sync::Arc<ContextSnapshot>),
}

/// A read-only view of a parent's context, cheap to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSnapshot {
    /// A rendering of what the parent knows.
    pub text: String,
}

/// A child could not be spawned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// The child asked for something the parent does not have.
    #[error(
        "sub-agent `{child}` asked for capabilities its parent does not hold. A child's grants may \
         only narrow: this is what stops a compromised sub-agent from escalating."
    )]
    WidensGrants {
        /// Which child.
        child: AgentId,
    },
    /// The child would hold all three powers at once.
    #[error(
        "sub-agent `{child}` would process untrusted input, reach private data, and be able to \
         change state or send data outward, all in one session. Drop one power, or escalate to a \
         human deliberately."
    )]
    RuleOfTwo {
        /// Which child.
        child: AgentId,
    },
}

/// What a child agent is allowed to be.
#[derive(Debug, Clone)]
pub struct Spawn {
    /// Which child.
    pub id: AgentId,
    /// What it inherits.
    pub inheritance: Inheritance,
    /// What it may do. Checked against the parent.
    pub grants: GrantSet,
}

/// A child agent, once it has been checked.
#[derive(Debug, Clone)]
pub struct Child {
    /// Where it sits in the tree.
    pub path: AgentPath,
    /// What it may do.
    pub grants: GrantSet,
    /// What it inherits.
    pub inheritance: Inheritance,
    /// Its powers, for the Rule of Two.
    pub powers: SessionPowers,
}

/// Check and create a child.
///
/// # Errors
/// Returns [`SpawnError::WidensGrants`] when the child asks for anything the parent lacks, and
/// [`SpawnError::RuleOfTwo`] when the resulting session would hold all three powers. Both are
/// refusals at spawn time rather than checks at use time, because by the time a capability is
/// exercised the decision has already been made.
pub fn spawn(
    parent_path: &AgentPath,
    parent_grants: &GrantSet,
    parent_powers: SessionPowers,
    request: Spawn,
) -> Result<Child, SpawnError> {
    if !request.grants.is_subset_of(parent_grants) {
        return Err(SpawnError::WidensGrants { child: request.id });
    }

    // Untrusted input flows downward: a parent that has read a fetched page cannot hand a child a
    // clean slate simply by summarising, because the summary is derived from that page.
    let mut powers = SessionPowers::from_grants(&request.grants);
    if parent_powers.untrusted_input && !matches!(request.inheritance, Inheritance::None) {
        powers = powers.observed_untrusted_input();
    }
    if powers.check().is_err() {
        return Err(SpawnError::RuleOfTwo { child: request.id });
    }

    Ok(Child {
        path: parent_path.child(request.id),
        grants: request.grants,
        inheritance: request.inheritance,
        powers,
    })
}

/// Label a child's output.
///
/// Low integrity, always. A sub-agent's reply is model output, and the fact that Frey wrote the
/// sub-agent does not make what the model said inside it true.
#[must_use]
pub fn label_result(child: &AgentPath, text: String) -> Untrusted<String> {
    Tainted::with_provenance(text, Provenance::new(format!("agent:{child}")))
}

/// Whether an event may be dropped when a consumer cannot keep up.
///
/// Presentation deltas may; semantics may not. A dropped delta makes a UI stutter, and a dropped
/// tool call makes the transcript a lie and replay diverge.
#[must_use]
pub fn droppable_under_pressure(event: &Event) -> bool {
    event.is_droppable()
}

/// Fan events from leaves up to the root, dropping only what may be dropped.
///
/// Returns the events that survived and how many were dropped, because "the UI looked laggy" should
/// be diagnosable from a number rather than from folklore.
#[must_use]
pub fn apply_backpressure(events: Vec<Event>, capacity: usize) -> (Vec<Event>, u64) {
    if events.len() <= capacity {
        return (events, 0);
    }
    let mut kept: Vec<Event> = Vec::with_capacity(capacity);
    let mut dropped = 0u64;

    // Semantic events are never sacrificed, even when that means exceeding the soft capacity:
    // going over budget is recoverable, and a transcript that lies is not.
    for event in events {
        if kept.len() < capacity || !droppable_under_pressure(&event) {
            kept.push(event);
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::capability::{Capability, Grant, HostPattern, PathScope, ProgramScope};
    use frey_core::event::EventKind;
    use frey_core::ids::{RunId, SeqId, ToolName};

    fn parent_grants() -> GrantSet {
        GrantSet::new([
            Grant::operator(Capability::FsRead(PathScope::new(["./"]).unwrap())),
            Grant::operator(Capability::Exec(ProgramScope::new(["git", "cargo"]))),
        ])
    }

    fn request(id: &str, grants: GrantSet) -> Spawn {
        Spawn {
            id: AgentId::new(id),
            inheritance: Inheritance::Summary("do the thing".into()),
            grants,
        }
    }

    #[test]
    fn a_child_may_narrow_its_parents_grants() {
        let narrower =
            GrantSet::new([Grant::operator(Capability::Exec(ProgramScope::new(["git"])))]);
        let child = spawn(
            &AgentPath::root(),
            &parent_grants(),
            SessionPowers::default(),
            request("builder", narrower),
        )
        .unwrap();
        assert_eq!(child.path.to_string(), "root/builder");
    }

    #[test]
    fn a_child_may_not_widen_them() {
        // The structural defence against escalation down a pipeline. A compromised sub-agent can
        // misbehave within its grants and no further, however convincing its output.
        let wider = GrantSet::new([Grant::operator(Capability::Exec(ProgramScope::new(["curl"])))]);
        let err = spawn(
            &AgentPath::root(),
            &parent_grants(),
            SessionPowers::default(),
            request("sneaky", wider),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::WidensGrants { .. }));
        assert!(format!("{err}").contains("may only narrow"));
    }

    #[test]
    fn untrusted_input_flows_downward_and_can_trip_the_rule_of_two() {
        // A parent that has read a fetched page cannot hand a child a clean slate by summarising:
        // the summary is derived from that page.
        let tainted_parent = SessionPowers::default().observed_untrusted_input();
        let dangerous = GrantSet::new([
            Grant::operator(Capability::FsRead(PathScope::new(["./"]).unwrap())),
            Grant::operator(Capability::NetEgress(HostPattern::new("api.test").unwrap())),
        ]);
        let parent = GrantSet::new([
            Grant::operator(Capability::FsRead(PathScope::new(["./"]).unwrap())),
            Grant::operator(Capability::NetEgress(HostPattern::new("api.test").unwrap())),
        ]);

        let err = spawn(
            &AgentPath::root(),
            &parent,
            tainted_parent,
            request("worker", dangerous.clone()),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::RuleOfTwo { .. }));
        assert!(format!("{err}").contains("Drop one power"));

        // The documented escape: a child that inherits nothing starts clean.
        let quarantined = Spawn {
            id: AgentId::new("quarantined"),
            inheritance: Inheritance::None,
            grants: dangerous,
        };
        assert!(spawn(&AgentPath::root(), &parent, tainted_parent, quarantined).is_ok());
    }

    #[test]
    fn a_snapshot_is_shared_rather_than_copied() {
        // Agents mostly want read-mostly context. Making them serialise it is the cost the actor
        // model charges for a guarantee they do not need.
        let snapshot = std::sync::Arc::new(ContextSnapshot { text: "a lot of context".into() });
        let child = spawn(
            &AgentPath::root(),
            &parent_grants(),
            SessionPowers::default(),
            Spawn {
                id: AgentId::new("reader"),
                inheritance: Inheritance::Snapshot(std::sync::Arc::clone(&snapshot)),
                grants: GrantSet::empty(),
            },
        )
        .unwrap();

        assert_eq!(std::sync::Arc::strong_count(&snapshot), 2, "shared, not deep-copied");
        assert!(matches!(child.inheritance, Inheritance::Snapshot(_)));
    }

    #[test]
    fn a_childs_output_is_low_integrity_whoever_wrote_the_child() {
        let path = AgentPath::root().child(AgentId::new("researcher"));
        let result = label_result(&path, "the answer is 42".into());
        assert_eq!(result.label().0, frey_core::taint::IntegrityLevel::Low);
        assert_eq!(result.provenance().origin.as_str(), "agent:root/researcher");
    }

    #[test]
    fn backpressure_drops_deltas_and_never_semantics() {
        let mut events = Vec::new();
        for i in 0..20u32 {
            events.push(Event::root(SeqId(i), EventKind::TextDelta { text: "x".into() }));
        }
        events.push(Event::root(
            SeqId(20),
            EventKind::ToolCallStarted {
                call: frey_core::ids::CallId::new("c1"),
                name: ToolName::new("fs_read"),
                args_preview: "{}".into(),
            },
        ));
        events.push(Event::root(SeqId(21), EventKind::RunStarted { run: RunId::new("r") }));

        let (kept, dropped) = apply_backpressure(events, 5);
        assert!(dropped > 0, "deltas must actually be dropped");
        assert_eq!(
            kept.iter().filter(|e| !e.is_droppable()).count(),
            2,
            "every semantic event survives, even past the soft capacity"
        );
    }

    #[test]
    fn nothing_is_dropped_when_the_consumer_keeps_up() {
        let events = vec![Event::root(SeqId::FIRST, EventKind::TextDelta { text: "hello".into() })];
        let (kept, dropped) = apply_backpressure(events, 10);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn grant_monotonicity_holds_all_the_way_down_a_deep_tree() {
        // The property the whole module exists for, checked at depth rather than once. Each rung
        // narrows: full grants, then exec only, then nothing.
        let rungs = [
            parent_grants(),
            GrantSet::new([Grant::operator(Capability::Exec(ProgramScope::new(["git"])))]),
            GrantSet::empty(),
        ];

        let mut grants = parent_grants();
        let mut path = AgentPath::root();
        let mut powers = SessionPowers::default();

        for (depth, rung) in rungs.iter().enumerate() {
            let child =
                spawn(&path, &grants, powers, request(&format!("d{depth}"), rung.clone())).unwrap();
            assert!(child.grants.is_subset_of(&grants), "depth {depth} widened");
            path = child.path;
            grants = child.grants;
            powers = child.powers;
        }
        assert_eq!(path.depth(), 3);

        // And the invariant bites at the bottom: a descendant of an empty grant set can acquire
        // nothing, however deep in the tree it sits.
        let err = spawn(
            &path,
            &grants,
            powers,
            request(
                "regains",
                GrantSet::new([Grant::operator(Capability::Exec(ProgramScope::new(["git"])))]),
            ),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::WidensGrants { .. }));
    }
}
