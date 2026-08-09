//! Deciding whether a run may proceed, and what it is told when it may not.
//!
//! All pure functions. The interesting paths here are the degraded and denied ones, and a healthy
//! CI machine cannot reproduce those by running — so they are unit tests over data instead, and
//! every platform exercises every case.

use frey_core::sandbox::{Availability, Degradation, ExecSpec, SandboxError, SandboxPolicy};

/// Whether a run may proceed, and under what confinement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Everything asked for can be enforced.
    Confined,
    /// Some controls are unavailable, and the operator has accepted that deliberately.
    Degraded {
        /// What could not be enforced, each with a reason.
        degradations: Vec<Degradation>,
    },
}

impl Decision {
    /// Whether anything was given up.
    #[must_use]
    pub fn is_fully_confined(&self) -> bool {
        matches!(self, Self::Confined)
    }
}

/// Check that a policy and a program are consistent before anything is spawned.
///
/// # Errors
/// Returns [`SandboxError::InvalidPolicy`] for a request that cannot be honoured whatever the
/// platform can do — an empty argv, a program outside the exec allowlist, or a working directory
/// outside the write scope. Catching these here means the failure names the mistake rather than
/// surfacing later as a confusing permission denial.
pub fn validate(spec: &ExecSpec, policy: &SandboxPolicy) -> Result<(), SandboxError> {
    let Some(program) = spec.program() else {
        return Err(SandboxError::InvalidPolicy("argv is empty; there is nothing to run".into()));
    };

    if !policy.exec.covers(program) {
        return Err(SandboxError::InvalidPolicy(format!(
            "`{program}` is not on the exec allowlist ({}). Add it deliberately, or use a tool \
             that does not shell out.",
            if policy.exec.programs().is_empty() {
                "which is empty".to_string()
            } else {
                policy.exec.programs().join(", ")
            }
        )));
    }

    let cwd = spec.cwd.to_string_lossy();
    if !policy.fs_write.covers(&cwd) && !policy.fs_read.covers(&cwd) {
        return Err(SandboxError::InvalidPolicy(format!(
            "the working directory `{cwd}` is outside every granted scope"
        )));
    }

    if spec.env.iter().any(|(name, _)| looks_like_a_secret(name)) {
        return Err(SandboxError::InvalidPolicy(
            "an environment variable that looks like a credential was passed to a sandboxed \
             process. Secrets are capability bindings resolved by the supervisor; a sandbox never \
             holds one."
                .into(),
        ));
    }

    Ok(())
}

fn looks_like_a_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["key", "token", "secret", "password", "credential"].iter().any(|p| lower.contains(p))
}

/// Decide whether a run may proceed on a host with the given capabilities.
///
/// # Errors
/// Returns [`SandboxError::CannotEnforce`] when a requested control is unavailable and degradation
/// was not explicitly accepted. This is the fail-closed path, and it is an error rather than a
/// warning because a process that runs unconfined after confinement was requested is precisely the
/// outcome the whole subsystem exists to prevent.
pub fn decide(
    policy: &SandboxPolicy,
    availability: &Availability,
    allow_degraded: bool,
) -> Result<Decision, SandboxError> {
    if !availability.usable {
        return Err(SandboxError::CannotEnforce {
            missing: policy.required_controls(),
            detail: availability.detail.clone(),
        });
    }

    let missing = availability.controls.missing_from(&policy.required_controls());
    if missing.is_empty() {
        return Ok(Decision::Confined);
    }

    if !allow_degraded {
        return Err(SandboxError::CannotEnforce { missing, detail: availability.detail.clone() });
    }

    Ok(Decision::Degraded {
        degradations: missing
            .into_iter()
            .map(|control| Degradation { control, reason: availability.detail.clone() })
            .collect(),
    })
}

/// Whether the operator accepted reduced confinement. Kept as a named function so the decision has
/// one place in the codebase and one place in a review.
#[must_use]
pub fn allow_degraded(configured: bool) -> bool {
    configured
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{LandlockAbi, linux_availability};
    use frey_core::capability::{HostPattern, PathScope, ProgramScope};
    use frey_core::sandbox::Control;
    use frey_core::sandbox::Limits;

    fn policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: PathScope::new(["./"]).unwrap(),
            fs_write: PathScope::new(["./out"]).unwrap(),
            egress: vec![HostPattern::new("api.github.com").unwrap()],
            exec: ProgramScope::new(["git", "cargo"]),
            limits: Limits::default(),
            copy_on_write: false,
        }
    }

    fn spec(argv: &[&str]) -> ExecSpec {
        ExecSpec::new(argv.iter().copied(), "./out")
    }

    #[test]
    fn a_host_that_cannot_enforce_the_policy_refuses_to_run_it() {
        // The whole point. A kernel without Landlock cannot scope the filesystem, so a request for
        // filesystem scoping must fail rather than produce an unconfined process.
        let old_kernel = linux_availability(LandlockAbi::NONE, true);
        let err = decide(&policy(), &old_kernel, false).unwrap_err();

        let SandboxError::CannotEnforce { missing, detail } = &err else {
            panic!("expected a refusal, got {err:?}")
        };
        assert!(missing.contains(&Control::FilesystemRead));
        assert!(missing.contains(&Control::NetworkEgress));
        assert!(detail.contains("landlock"), "{detail}");
        assert!(format!("{err}").contains("refusing to run unconfined"));
    }

    #[test]
    fn degradation_requires_a_deliberate_decision_and_is_recorded() {
        let old_kernel = linux_availability(LandlockAbi::NONE, true);
        let decision = decide(&policy(), &old_kernel, true).unwrap();

        let Decision::Degraded { degradations } = &decision else { panic!("expected degradation") };
        assert!(!decision.is_fully_confined(), "a degraded run must never look like a clean one");
        assert!(degradations.iter().all(|d| d.reason.contains("landlock")));
    }

    #[test]
    fn a_capable_host_runs_fully_confined() {
        let modern = linux_availability(LandlockAbi::FULL, true);
        assert_eq!(decide(&policy(), &modern, false).unwrap(), Decision::Confined);
    }

    #[test]
    fn a_partial_abi_is_reported_precisely_rather_than_as_all_or_nothing() {
        // ABI 1 scopes the filesystem but not ports. Reporting that as "unavailable" would push an
        // operator toward disabling confinement entirely.
        let partial = linux_availability(LandlockAbi(1), true);
        let err = decide(&policy(), &partial, false).unwrap_err();
        let SandboxError::CannotEnforce { missing, .. } = err else { panic!("expected refusal") };
        assert_eq!(missing, vec![Control::NetworkEgress], "only the port scoping is missing");
    }

    #[test]
    fn a_program_off_the_allowlist_is_rejected_before_anything_is_spawned() {
        let err = validate(&spec(&["curl", "https://evil.test"]), &policy()).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("curl"), "{message}");
        assert!(message.contains("cargo, git"), "name what is allowed: {message}");
    }

    #[test]
    fn obfuscation_has_nothing_to_work_on_because_the_program_is_an_argv_element() {
        // The reason the shell tool takes argv rather than a command string: there is no string to
        // obfuscate. `sh -c "r''m -rf /"` fails on `sh`, before the payload is ever considered.
        assert!(validate(&spec(&["sh", "-c", "r''m -rf /"]), &policy()).is_err());
        assert!(validate(&spec(&["git", "status"]), &policy()).is_ok());
    }

    #[test]
    fn an_empty_argv_is_a_policy_error_not_a_crash() {
        assert!(matches!(validate(&spec(&[]), &policy()), Err(SandboxError::InvalidPolicy(_))));
    }

    #[test]
    fn a_working_directory_outside_every_scope_is_rejected() {
        let outside = ExecSpec::new(["git"], "/etc");
        let err = validate(&outside, &policy()).unwrap_err();
        assert!(format!("{err}").contains("outside every granted scope"));
    }

    #[test]
    fn a_credential_in_the_environment_is_refused() {
        // A sandbox never holds a secret: the supervisor performs the authenticated call on the
        // tool's behalf. Passing one in would defeat that, silently.
        let mut leaky = spec(&["git", "push"]);
        leaky.env.push(("GITHUB_TOKEN".into(), "ghp_real".into()));
        let err = validate(&leaky, &policy()).unwrap_err();
        assert!(format!("{err}").contains("capability bindings"), "{err}");

        let mut fine = spec(&["git", "status"]);
        fine.env.push(("LANG".into(), "C".into()));
        assert!(validate(&fine, &policy()).is_ok());
    }

    #[test]
    fn an_unusable_backend_refuses_regardless_of_the_degraded_flag_being_unset() {
        let unusable = Availability::unusable("no backend on this host");
        assert!(decide(&policy(), &unusable, false).is_err());
    }
}
