//! Capabilities: what an agent is allowed to reach.
//!
//! Two invariants make this worth having, and both are property-tested:
//!
//! 1. **No ambient authority.** A tool that did not declare a capability cannot obtain one.
//! 2. **Monotonic narrowing.** A child agent's grants are always a subset of its parent's. This is
//!    the structural defence against the multi-agent privilege escalation that the injection
//!    literature calls out as growing multiplicatively with pipeline depth.
//!
//! On top of those sits the [Rule of Two][SessionPowers]: within one session an agent may hold at
//! most two of {processes untrusted input, reaches private data, changes state or communicates
//! externally}.
//!
//! # Scope of this module
//!
//! These types describe *intent*. Enforcement lives in `frey-sandbox` and the policy layer of
//! `frey-tools`. In particular [`PathScope`] matching here is **lexical**; canonicalisation,
//! symlink resolution, and the actual kernel-level restriction happen at the enforcement point.
//! Anything in this module is a necessary condition, never a sufficient one.

use std::fmt;

use smol_str::SmolStr;

use crate::ids::{AgentId, ServerId};

/// A set of filesystem path prefixes.
///
/// Constructed through [`PathScope::new`], which rejects any prefix containing a `..` component,
/// because a scope that can walk upward is not a scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PathScope {
    prefixes: Vec<SmolStr>,
}

/// A path prefix was not usable as a scope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// The prefix contained a parent-directory component.
    #[error(
        "path scope `{0}` contains a `..` component; a scope that can walk upward is not a scope"
    )]
    ParentTraversal(SmolStr),
    /// The prefix was empty.
    #[error("path scope prefixes must not be empty")]
    Empty,
}

impl PathScope {
    /// A scope covering the given prefixes.
    ///
    /// # Errors
    /// Returns [`ScopeError`] if any prefix is empty or contains a `..` component.
    pub fn new<I, S>(prefixes: I) -> Result<Self, ScopeError>
    where
        I: IntoIterator<Item = S>,
        S: Into<SmolStr>,
    {
        let mut out = Vec::new();
        for prefix in prefixes {
            let p: SmolStr = prefix.into();
            if p.is_empty() {
                return Err(ScopeError::Empty);
            }
            if p.split(['/', '\\']).any(|c| c == "..") {
                return Err(ScopeError::ParentTraversal(p));
            }
            out.push(normalise(&p));
        }
        out.sort_unstable();
        out.dedup();
        Ok(Self { prefixes: out })
    }

    /// A scope covering nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this scope covers nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Whether `path` falls under one of the prefixes.
    ///
    /// Matching is on whole path components, so `./src` does not cover `./srcret`.
    #[must_use]
    pub fn covers(&self, path: &str) -> bool {
        let path = normalise(path);
        self.prefixes.iter().any(|prefix| covers_lexically(prefix, &path))
    }

    /// Whether every path this scope covers is also covered by `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.prefixes.iter().all(|p| other.prefixes.iter().any(|q| covers_lexically(q, p)))
    }

    /// The prefixes, normalised and sorted.
    #[must_use]
    pub fn prefixes(&self) -> &[SmolStr] {
        &self.prefixes
    }
}

fn normalise(path: &str) -> SmolStr {
    let replaced = path.replace('\\', "/");
    let trimmed = replaced.strip_prefix("./").unwrap_or(&replaced);
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() { SmolStr::new("/") } else { SmolStr::new(trimmed) }
}

fn covers_lexically(prefix: &str, path: &str) -> bool {
    if prefix == "/" || prefix == path {
        return true;
    }
    path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

/// A network destination an agent may reach.
///
/// Deliberately **concrete hostnames only**. Wildcards are rejected at construction, because the
/// enforcement layer resolves each host exactly once when the sandbox starts — which is what closes
/// DNS rebinding — and a wildcard cannot be resolved ahead of time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostPattern(SmolStr);

/// A host was not usable in an egress allowlist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The host contained a wildcard.
    #[error(
        "egress host `{0}` contains a wildcard; hosts are resolved once at sandbox start to close \
         DNS rebinding, so they must be concrete"
    )]
    Wildcard(String),
    /// The host was empty or contained a scheme, port, or path.
    #[error("egress host `{0}` must be a bare hostname, with no scheme, port, or path")]
    NotBare(String),
}

impl HostPattern {
    /// A concrete hostname.
    ///
    /// # Errors
    /// Returns [`HostError`] for wildcards or anything that is not a bare hostname.
    pub fn new(host: impl Into<String>) -> Result<Self, HostError> {
        let host = host.into();
        if host.contains('*') {
            return Err(HostError::Wildcard(host));
        }
        if host.is_empty() || host.contains("://") || host.contains('/') || host.contains(':') {
            return Err(HostError::NotBare(host));
        }
        Ok(Self(SmolStr::new(host.to_ascii_lowercase())))
    }

    /// The hostname.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<String> for HostPattern {
    type Error = HostError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<HostPattern> for String {
    fn from(value: HostPattern) -> Self {
        value.0.to_string()
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// Programs an agent may execute.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProgramScope {
    programs: Vec<SmolStr>,
}

impl ProgramScope {
    /// A scope covering the named programs.
    pub fn new<I, S>(programs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<SmolStr>,
    {
        let mut programs: Vec<SmolStr> = programs.into_iter().map(Into::into).collect();
        programs.sort_unstable();
        programs.dedup();
        Self { programs }
    }

    /// Whether `program` may be executed. Compared as a whole argv element, never as a substring of
    /// a rendered command line — which is why obfuscations that defeat regex denylists are
    /// irrelevant here.
    #[must_use]
    pub fn covers(&self, program: &str) -> bool {
        self.programs.iter().any(|p| p == program)
    }

    /// Whether every program in this scope is in `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.programs.iter().all(|p| other.covers(p))
    }

    /// The programs.
    #[must_use]
    pub fn programs(&self) -> &[SmolStr] {
        &self.programs
    }
}

/// A spending limit, in millionths of a unit of currency, so budgets compare exactly.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Budget {
    /// Micro-units, e.g. 1_000_000 == 1 USD.
    pub micros: u64,
}

impl Budget {
    /// A budget of `units` whole currency units.
    #[must_use]
    pub fn units(units: u64) -> Self {
        Self { micros: units.saturating_mul(1_000_000) }
    }
}

/// A name under which a secret is registered. The value never appears in a capability.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SecretName(pub SmolStr);

/// A glob-free tool selector for MCP grants: either every tool on a server, or named ones.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolSelector {
    /// Every tool the server offers.
    All,
    /// Only these.
    Named {
        /// Tool names, as the server spells them.
        names: Vec<SmolStr>,
    },
}

impl ToolSelector {
    /// Whether `name` is selected.
    #[must_use]
    pub fn covers(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Named { names } => names.iter().any(|n| n == name),
        }
    }

    /// Whether everything this selector permits is also permitted by `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        match (self, other) {
            (_, Self::All) => true,
            (Self::All, Self::Named { .. }) => false,
            (Self::Named { names }, other) => names.iter().all(|n| other.covers(n)),
        }
    }
}

/// One thing an agent is permitted to do.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Read files under a scope.
    FsRead(PathScope),
    /// Write files under a scope.
    FsWrite(PathScope),
    /// Make outbound requests to a host.
    NetEgress(HostPattern),
    /// Execute programs.
    Exec(ProgramScope),
    /// Use a secret. The supervisor performs the authenticated call; the holder never sees the
    /// value.
    Secret(SecretName),
    /// Spend money.
    Spend(Budget),
    /// Call tools on an MCP server.
    Mcp {
        /// Which server.
        server: ServerId,
        /// Which of its tools.
        tools: ToolSelector,
    },
    /// Delegate to another agent.
    Delegate(AgentId),
}

impl Capability {
    /// Whether holding `other` implies holding `self`.
    ///
    /// This is the ordering that makes [`GrantSet::is_subset_of`] a partial order, and therefore
    /// the ordering that makes child-narrows-parent checkable.
    #[must_use]
    pub fn is_covered_by(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::FsRead(a), Self::FsRead(b)) | (Self::FsWrite(a), Self::FsWrite(b)) => {
                a.is_subset_of(b)
            }
            // Being able to write implies being able to read the same scope.
            (Self::FsRead(a), Self::FsWrite(b)) => a.is_subset_of(b),
            (Self::NetEgress(a), Self::NetEgress(b)) => a == b,
            (Self::Exec(a), Self::Exec(b)) => a.is_subset_of(b),
            (Self::Secret(a), Self::Secret(b)) => a == b,
            (Self::Spend(a), Self::Spend(b)) => a.micros <= b.micros,
            (Self::Mcp { server: s1, tools: t1 }, Self::Mcp { server: s2, tools: t2 }) => {
                s1 == s2 && t1.is_subset_of(t2)
            }
            (Self::Delegate(a), Self::Delegate(b)) => a == b,
            _ => false,
        }
    }

    /// Whether exercising this capability changes state outside the process or sends data out of
    /// it. Feeds the Rule of Two and the default risk classification.
    #[must_use]
    pub fn is_mutating_or_egress(&self) -> bool {
        matches!(
            self,
            Self::FsWrite(_)
                | Self::NetEgress(_)
                | Self::Exec(_)
                | Self::Spend(_)
                | Self::Delegate(_)
        )
    }

    /// Whether this capability can reach data the operator would consider private.
    #[must_use]
    pub fn is_confidential_access(&self) -> bool {
        matches!(self, Self::FsRead(_) | Self::Secret(_) | Self::Mcp { .. })
    }
}

/// Who authorised a grant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Authority {
    /// Written in configuration or code by the operator.
    Operator,
    /// A human approved it at runtime.
    HumanApproval {
        /// The approval record's id.
        id: SmolStr,
    },
    /// A deterministic policy rule allowed it.
    Policy {
        /// Which rule.
        rule: SmolStr,
    },
    /// Inherited from a parent agent. Can only ever narrow.
    Parent {
        /// Which agent.
        agent: AgentId,
    },
}

/// A capability, plus who authorised it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    /// What is permitted.
    pub capability: Capability,
    /// Who said so.
    pub authority: Authority,
}

impl Grant {
    /// A grant authorised by the operator.
    #[must_use]
    pub fn operator(capability: Capability) -> Self {
        Self { capability, authority: Authority::Operator }
    }
}

/// Everything an agent is permitted to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct GrantSet {
    grants: Vec<Grant>,
}

impl GrantSet {
    /// An agent with no permissions at all. The correct default.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// A set containing `grants`.
    pub fn new(grants: impl IntoIterator<Item = Grant>) -> Self {
        Self { grants: grants.into_iter().collect() }
    }

    /// Add a grant.
    pub fn insert(&mut self, grant: Grant) {
        self.grants.push(grant);
    }

    /// Whether `wanted` is permitted by any grant in this set.
    #[must_use]
    pub fn permits(&self, wanted: &Capability) -> bool {
        self.grants.iter().any(|g| wanted.is_covered_by(&g.capability))
    }

    /// Whether every capability in this set is permitted by `other`.
    ///
    /// This is the child-narrows-parent check, run at spawn time.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.grants.iter().all(|g| other.permits(&g.capability))
    }

    /// The grants.
    #[must_use]
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Whether any grant reaches private data.
    #[must_use]
    pub fn has_confidential_access(&self) -> bool {
        self.grants.iter().any(|g| g.capability.is_confidential_access())
    }

    /// Whether any grant changes state or leaves the machine.
    #[must_use]
    pub fn has_mutating_egress(&self) -> bool {
        self.grants.iter().any(|g| g.capability.is_mutating_or_egress())
    }
}

/// The three powers the Rule of Two counts.
///
/// Meta's formulation: within a single session an agent should hold at most two of these. Holding
/// all three means an attacker who controls the untrusted content can read private data and ship it
/// out, with no exploit code required.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionPowers {
    /// The session has processed content Frey did not author.
    pub untrusted_input: bool,
    /// The session can reach private data.
    pub confidential_access: bool,
    /// The session can change state or communicate externally.
    pub mutating_egress: bool,
}

/// The session holds all three powers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "this session processes untrusted input, reaches private data, and can change state or send \
     data outward. Hold at most two: fork a session that drops one power, or escalate to a human."
)]
pub struct RuleOfTwoViolation;

impl SessionPowers {
    /// Powers implied by a grant set, before any untrusted input has been seen.
    #[must_use]
    pub fn from_grants(grants: &GrantSet) -> Self {
        Self {
            untrusted_input: false,
            confidential_access: grants.has_confidential_access(),
            mutating_egress: grants.has_mutating_egress(),
        }
    }

    /// Note that the session has now seen content Frey did not author.
    #[must_use]
    pub fn observed_untrusted_input(mut self) -> Self {
        self.untrusted_input = true;
        self
    }

    /// How many of the three powers are held.
    #[must_use]
    pub fn count(self) -> u8 {
        u8::from(self.untrusted_input)
            + u8::from(self.confidential_access)
            + u8::from(self.mutating_egress)
    }

    /// Whether this combination is permitted.
    ///
    /// # Errors
    /// Returns [`RuleOfTwoViolation`] when all three powers are held.
    pub fn check(self) -> Result<(), RuleOfTwoViolation> {
        if self.count() == 3 { Err(RuleOfTwoViolation) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(prefixes: &[&str]) -> PathScope {
        PathScope::new(prefixes.iter().copied()).expect("valid scope")
    }

    #[test]
    fn path_scopes_reject_upward_traversal_at_construction() {
        assert!(matches!(PathScope::new(["../etc"]), Err(ScopeError::ParentTraversal(_))));
        assert!(matches!(PathScope::new(["src/../../etc"]), Err(ScopeError::ParentTraversal(_))));
        assert!(matches!(PathScope::new([""]), Err(ScopeError::Empty)));
    }

    #[test]
    fn path_scopes_match_whole_components() {
        let s = scope(&["./src"]);
        assert!(s.covers("src/main.rs"));
        assert!(s.covers("src"));
        assert!(
            !s.covers("srcret/secrets.txt"),
            "prefix matching must respect component boundaries"
        );
        assert!(!s.covers("other/src/main.rs"));
    }

    #[test]
    fn path_scope_subset_is_a_partial_order() {
        let root = scope(&["/"]);
        let all = scope(&["./"]);
        let src = scope(&["./src"]);
        let deep = scope(&["./src/bin"]);

        assert!(src.is_subset_of(&src), "reflexive");
        assert!(deep.is_subset_of(&src));
        assert!(src.is_subset_of(&all));
        assert!(deep.is_subset_of(&all), "transitive");
        assert!(!src.is_subset_of(&deep), "antisymmetric for distinct scopes");
        assert!(all.is_subset_of(&root));
    }

    #[test]
    fn egress_hosts_must_be_concrete() {
        assert!(matches!(HostPattern::new("*.github.com"), Err(HostError::Wildcard(_))));
        assert!(matches!(HostPattern::new("https://api.github.com"), Err(HostError::NotBare(_))));
        assert!(matches!(HostPattern::new("api.github.com:443"), Err(HostError::NotBare(_))));
        assert!(matches!(HostPattern::new("api.github.com/repos"), Err(HostError::NotBare(_))));
        assert_eq!(HostPattern::new("API.GitHub.com").unwrap().as_str(), "api.github.com");
    }

    #[test]
    fn writing_implies_reading_the_same_scope() {
        let write_all = Capability::FsWrite(scope(&["./"]));
        let read_src = Capability::FsRead(scope(&["./src"]));
        assert!(read_src.is_covered_by(&write_all));

        let write_src = Capability::FsWrite(scope(&["./src"]));
        let read_all = Capability::FsRead(scope(&["./"]));
        assert!(!write_src.is_covered_by(&read_all), "reading does not imply writing");
    }

    #[test]
    fn grants_are_denied_by_default() {
        let none = GrantSet::empty();
        assert!(!none.permits(&Capability::FsRead(scope(&["./"]))));
        assert!(!none.permits(&Capability::Exec(ProgramScope::new(["git"]))));
    }

    #[test]
    fn child_grants_must_narrow_never_widen() {
        let parent = GrantSet::new([
            Grant::operator(Capability::FsRead(scope(&["./"]))),
            Grant::operator(Capability::Exec(ProgramScope::new(["git", "cargo", "rg"]))),
        ]);

        let narrower = GrantSet::new([
            Grant::operator(Capability::FsRead(scope(&["./src"]))),
            Grant::operator(Capability::Exec(ProgramScope::new(["git"]))),
        ]);
        assert!(narrower.is_subset_of(&parent));

        let sideways =
            GrantSet::new([Grant::operator(Capability::Exec(ProgramScope::new(["curl"])))]);
        assert!(
            !sideways.is_subset_of(&parent),
            "a child cannot acquire a program the parent lacks"
        );

        let wider = GrantSet::new([Grant::operator(Capability::FsWrite(scope(&["./"])))]);
        assert!(!wider.is_subset_of(&parent), "a child cannot upgrade read to write");
    }

    #[test]
    fn subset_is_reflexive_and_transitive() {
        let a = GrantSet::new([Grant::operator(Capability::FsRead(scope(&["./src/bin"])))]);
        let b = GrantSet::new([Grant::operator(Capability::FsRead(scope(&["./src"])))]);
        let c = GrantSet::new([Grant::operator(Capability::FsRead(scope(&["./"])))]);

        assert!(a.is_subset_of(&a));
        assert!(a.is_subset_of(&b));
        assert!(b.is_subset_of(&c));
        assert!(a.is_subset_of(&c), "transitive");
    }

    #[test]
    fn spend_budgets_narrow_downward() {
        let parent = GrantSet::new([Grant::operator(Capability::Spend(Budget::units(10)))]);
        let child = GrantSet::new([Grant::operator(Capability::Spend(Budget::units(1)))]);
        let greedy = GrantSet::new([Grant::operator(Capability::Spend(Budget::units(100)))]);
        assert!(child.is_subset_of(&parent));
        assert!(!greedy.is_subset_of(&parent));
    }

    #[test]
    fn mcp_selectors_narrow_from_all_to_named() {
        let server = ServerId::new("github");
        let parent = GrantSet::new([Grant::operator(Capability::Mcp {
            server: server.clone(),
            tools: ToolSelector::All,
        })]);
        let child = GrantSet::new([Grant::operator(Capability::Mcp {
            server: server.clone(),
            tools: ToolSelector::Named { names: vec!["list_issues".into()] },
        })]);
        assert!(child.is_subset_of(&parent));
        assert!(!parent.is_subset_of(&child));

        let other_server = GrantSet::new([Grant::operator(Capability::Mcp {
            server: ServerId::new("slack"),
            tools: ToolSelector::All,
        })]);
        assert!(!other_server.is_subset_of(&parent));
    }

    #[test]
    fn rule_of_two_rejects_exactly_the_all_three_case() {
        let two_of_three = [
            SessionPowers {
                untrusted_input: true,
                confidential_access: true,
                mutating_egress: false,
            },
            SessionPowers {
                untrusted_input: true,
                confidential_access: false,
                mutating_egress: true,
            },
            SessionPowers {
                untrusted_input: false,
                confidential_access: true,
                mutating_egress: true,
            },
        ];
        for powers in two_of_three {
            assert!(powers.check().is_ok(), "two powers is permitted: {powers:?}");
        }

        let all_three = SessionPowers {
            untrusted_input: true,
            confidential_access: true,
            mutating_egress: true,
        };
        assert_eq!(all_three.check(), Err(RuleOfTwoViolation));
        assert_eq!(all_three.count(), 3);
    }

    #[test]
    fn reading_a_fetched_page_is_what_trips_the_rule() {
        // A perfectly ordinary agent: reads the repo, can run git, can call an API.
        let grants = GrantSet::new([
            Grant::operator(Capability::FsRead(scope(&["./"]))),
            Grant::operator(Capability::NetEgress(HostPattern::new("api.github.com").unwrap())),
        ]);
        let powers = SessionPowers::from_grants(&grants);
        assert!(powers.check().is_ok(), "safe until it reads something an attacker wrote");

        let after_fetch = powers.observed_untrusted_input();
        assert_eq!(after_fetch.check(), Err(RuleOfTwoViolation));
    }
}
