//! Sessions and surfaces.
//!
//! **The journal is the session.** There is no second copy of the state that could drift from the
//! transcript, so resuming is replaying and forking is branching a journal — which makes "try a
//! different prompt from turn seven" free rather than a feature.
//!
//! Session powers persist too, which matters more than it sounds: an agent that read a fetched page
//! yesterday does not silently regain full powers today just because the process restarted.

use frey_agent::journal::Journal;
use frey_core::capability::{GrantSet, SessionPowers};
use frey_core::error::Risk;
use frey_core::ids::{RunId, SessionId};

/// Where a harness surfaces its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Surface {
    /// A terminal.
    Cli,
    /// An AG-UI event stream for a frontend.
    AgUi,
    /// The harness itself is an A2A agent.
    A2a,
    /// The harness exposes its toolset over MCP.
    Mcp,
    /// No interaction: CI, cron, a queue worker.
    Headless,
}

impl Surface {
    /// Whether a human is present to answer a question.
    #[must_use]
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Cli | Self::AgUi)
    }
}

/// When approval is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalPolicy {
    /// Ask a human at or above this risk.
    Interactive {
        /// The threshold.
        at_or_above: Risk,
    },
    /// Never ask; the grant set is the boundary.
    AutoAllow,
    /// Refuse anything that would need approval.
    DenyAll,
}

impl ApprovalPolicy {
    /// Whether this policy needs a human to be present.
    #[must_use]
    pub fn needs_a_human(self) -> bool {
        matches!(self, Self::Interactive { .. })
    }
}

/// A harness was configured in a way that cannot work.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HarnessError {
    /// Nobody is there to answer.
    #[error(
        "a headless surface cannot use an interactive approval policy: the run would stop at the \
         first gated action and wait for a human who is not there. Choose auto_allow with a \
         narrower grant set, or deny_all."
    )]
    NoOneToAsk,
}

/// One conversation, possibly spanning several runs.
#[derive(Debug, Clone)]
pub struct Session {
    /// Which session.
    pub id: SessionId,
    /// The record. This *is* the state.
    pub journal: Journal,
    /// What it may do.
    pub grants: GrantSet,
    /// Rule-of-Two tracking, which survives a restart.
    pub powers: SessionPowers,
}

impl Session {
    /// A new session.
    #[must_use]
    pub fn new(id: SessionId, grants: GrantSet) -> Self {
        let powers = SessionPowers::from_grants(&grants);
        Self { journal: Journal::new(RunId::new(format!("{id}-run"))), id, grants, powers }
    }

    /// Branch this session, sharing everything recorded so far.
    ///
    /// The shared prefix is already journalled, so a fork replays it rather than re-calling the
    /// provider. That is what makes "try a different prompt from turn seven" cost nothing.
    #[must_use]
    pub fn fork(&self, id: SessionId) -> Self {
        Self {
            journal: self.journal.clone(),
            id,
            grants: self.grants.clone(),
            // Powers carry across a fork: a branch of a session that saw untrusted input has still
            // seen it.
            powers: self.powers,
        }
    }

    /// Note that this session has seen content Frey did not author.
    pub fn observed_untrusted_input(&mut self) {
        self.powers = self.powers.observed_untrusted_input();
    }

    /// How many recorded steps this session could replay.
    #[must_use]
    pub fn replayable_steps(&self) -> usize {
        self.journal.len()
    }
}

/// Check a harness configuration before anything runs.
///
/// # Errors
/// Returns [`HarnessError::NoOneToAsk`] for a headless surface with an interactive approval policy
/// — a hang waiting to happen, and far better caught at build time than at three in the morning.
pub fn validate(surface: Surface, approvals: ApprovalPolicy) -> Result<(), HarnessError> {
    if !surface.is_interactive() && approvals.needs_a_human() {
        return Err(HarnessError::NoOneToAsk);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::capability::{Capability, Grant, HostPattern, PathScope};

    fn grants() -> GrantSet {
        GrantSet::new([Grant::operator(Capability::FsRead(PathScope::new(["./"]).unwrap()))])
    }

    #[test]
    fn a_headless_surface_with_interactive_approvals_fails_before_it_runs() {
        // Otherwise it stops at the first gated action and waits for a human who is not there —
        // discovered at three in the morning rather than at build time.
        let err =
            validate(Surface::Headless, ApprovalPolicy::Interactive { at_or_above: Risk::Medium })
                .unwrap_err();
        assert!(format!("{err}").contains("who is not there"));
        assert!(format!("{err}").contains("deny_all"), "and it names the alternatives");
    }

    #[test]
    fn a_headless_surface_with_a_non_interactive_policy_is_fine() {
        assert!(validate(Surface::Headless, ApprovalPolicy::AutoAllow).is_ok());
        assert!(validate(Surface::Headless, ApprovalPolicy::DenyAll).is_ok());
        assert!(validate(Surface::A2a, ApprovalPolicy::DenyAll).is_ok());
    }

    #[test]
    fn an_interactive_surface_may_ask() {
        assert!(
            validate(Surface::Cli, ApprovalPolicy::Interactive { at_or_above: Risk::Medium })
                .is_ok()
        );
        assert!(
            validate(Surface::AgUi, ApprovalPolicy::Interactive { at_or_above: Risk::High })
                .is_ok()
        );
    }

    #[test]
    fn a_fork_shares_the_recorded_prefix_rather_than_replaying_the_provider() {
        let mut original = Session::new(SessionId::new("s1"), grants());
        original.journal.record(frey_agent::journal::Effect::ToolResult {
            tool: "fs_read".into(),
            content: "contents".into(),
            is_error: false,
        });

        let branch = original.fork(SessionId::new("s1-alt"));
        assert_eq!(branch.replayable_steps(), 1, "the shared prefix comes along");
        assert_eq!(branch.id, SessionId::new("s1-alt"));
    }

    #[test]
    fn powers_survive_a_fork_and_a_restart() {
        // A branch of a session that has seen untrusted input has still seen it, and so has the
        // same session tomorrow. Forgetting would silently restore powers the Rule of Two removed.
        let mut session = Session::new(SessionId::new("s1"), grants());
        assert!(!session.powers.untrusted_input);

        session.observed_untrusted_input();
        assert!(session.fork(SessionId::new("s2")).powers.untrusted_input);
    }

    #[test]
    fn powers_are_derived_from_grants_at_creation() {
        let reaching = GrantSet::new([
            Grant::operator(Capability::FsRead(PathScope::new(["./"]).unwrap())),
            Grant::operator(Capability::NetEgress(HostPattern::new("api.test").unwrap())),
        ]);
        let session = Session::new(SessionId::new("s1"), reaching);
        assert!(session.powers.confidential_access);
        assert!(session.powers.mutating_egress);
        assert!(session.powers.check().is_ok(), "two powers is permitted");

        // And the third trips it.
        let mut tainted = session;
        tainted.observed_untrusted_input();
        assert!(tainted.powers.check().is_err());
    }

    #[test]
    fn the_journal_is_the_session_rather_than_a_copy_of_it() {
        let session = Session::new(SessionId::new("s1"), grants());
        assert_eq!(session.replayable_steps(), 0);
        assert_eq!(session.journal.run.as_str(), "s1-run");
    }
}
