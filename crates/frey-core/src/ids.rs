//! Identifiers.
//!
//! Every id in Frey is a newtype, because a `String` that could be a run id, a tool name, or a
//! provider id is a bug waiting for a refactor.
//!
//! **Ids are never random.** Run, turn, and segment ids are derived from the journal's monotonic
//! sequence so that replaying a recorded run produces identical ids. A `Uuid::new_v4()` anywhere in
//! this crate would silently break determinism, which is why there is no random source here at all.

use std::fmt;

use smol_str::SmolStr;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(SmolStr);

        impl $name {
            /// Wrap a string as this id.
            pub fn new(value: impl Into<SmolStr>) -> Self {
                Self(value.into())
            }

            /// The underlying text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.0.as_str())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.into())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value.into())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.0.as_str()
            }
        }
    };
}

macro_rules! counter_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u32);

        impl $name {
            /// The first id in a sequence.
            pub const FIRST: Self = Self(0);

            /// The next id in the sequence.
            #[must_use]
            pub fn next(self) -> Self {
                Self(self.0 + 1)
            }

            /// The raw index.
            #[must_use]
            pub fn index(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

string_id!(
    /// Identifies one agent run.
    RunId
);
string_id!(
    /// Identifies a conversation that may span several runs. Maps to A2A's `contextId`.
    SessionId
);
string_id!(
    /// A provider-supplied tool-call correlation id: Anthropic's `tool_use.id`, OpenAI's `call_id`.
    CallId
);
string_id!(
    /// The name a tool is presented under, after namespacing.
    ToolName
);
string_id!(
    /// Identifies a provider adapter, e.g. `anthropic`, `openrouter`, or a config-defined name.
    ProviderId
);
string_id!(
    /// A model identifier as the provider spells it, e.g. `claude-opus-5`.
    ModelId
);
string_id!(
    /// Identifies a configured MCP server.
    ServerId
);
string_id!(
    /// Identifies a sub-agent, delegated agent, or A2A peer.
    AgentId
);
string_id!(
    /// Identifies a skill.
    SkillId
);

counter_id!(
    /// A turn within a run.
    TurnId,
    "turn-"
);
counter_id!(
    /// A segment within a rendered prompt.
    SegmentId,
    "seg-"
);
counter_id!(
    /// A position in the run journal. The source of determinism for replay.
    SeqId,
    "seq-"
);

/// Where an agent sits in the run tree, e.g. `root/researcher/fetcher`.
///
/// Carried on every event so a UI can render nested progress without the framework prescribing a
/// layout, and so a trace can be reassembled from the journal alone.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct AgentPath(Vec<AgentId>);

impl AgentPath {
    /// The path of the root agent: empty.
    #[must_use]
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// The path of a child of this agent.
    #[must_use]
    pub fn child(&self, id: AgentId) -> Self {
        let mut next = self.0.clone();
        next.push(id);
        Self(next)
    }

    /// How deep this agent is. The root is 0.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// Whether this path is the root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The path of this agent's parent, or `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.0.is_empty() { None } else { Some(Self(self.0[..self.0.len() - 1].to_vec())) }
    }

    /// Whether `self` is `other` or one of its ancestors. Used to scope cancellation and to check
    /// that a capability grant is being narrowed down the tree rather than sideways.
    #[must_use]
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        other.0.len() >= self.0.len() && other.0[..self.0.len()] == self.0[..]
    }

    /// The agent ids, root first.
    #[must_use]
    pub fn segments(&self) -> &[AgentId] {
        &self.0
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("root")?;
        for id in &self.0 {
            write!(f, "/{id}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_ids_are_sequential_and_never_random() {
        let a = TurnId::FIRST;
        let b = a.next();
        assert_eq!(b.index(), 1);
        assert_eq!(a.next(), b, "the same input always produces the same next id");
        assert_eq!(b.to_string(), "turn-1");
    }

    #[test]
    fn string_ids_round_trip_through_serde_transparently() {
        let id = ToolName::new("github_list_issues");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"github_list_issues\"", "ids serialise as bare strings");
        assert_eq!(serde_json::from_str::<ToolName>(&json).unwrap(), id);
    }

    #[test]
    fn agent_paths_nest_and_render() {
        let root = AgentPath::root();
        assert!(root.is_root());
        assert_eq!(root.to_string(), "root");
        assert_eq!(root.parent(), None);

        let researcher = root.child(AgentId::new("researcher"));
        let fetcher = researcher.child(AgentId::new("fetcher"));
        assert_eq!(fetcher.to_string(), "root/researcher/fetcher");
        assert_eq!(fetcher.depth(), 2);
        assert_eq!(fetcher.parent().as_ref(), Some(&researcher));
    }

    #[test]
    fn ancestry_is_reflexive_and_directional() {
        let root = AgentPath::root();
        let a = root.child(AgentId::new("a"));
        let b = root.child(AgentId::new("b"));
        let a_child = a.child(AgentId::new("c"));

        assert!(root.is_ancestor_of(&a_child), "the root is an ancestor of everything");
        assert!(a.is_ancestor_of(&a_child));
        assert!(a.is_ancestor_of(&a), "reflexive");
        assert!(!b.is_ancestor_of(&a_child), "siblings are not ancestors");
        assert!(!a_child.is_ancestor_of(&a), "descendants are not ancestors of their parents");
    }
}
