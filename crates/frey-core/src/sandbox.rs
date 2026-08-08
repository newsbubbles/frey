//! What a sandbox must promise, and what it must confess.
//!
//! Every backend — Landlock on Linux, Seatbelt on macOS, AppContainer or a restricted token on
//! Windows, or a WebAssembly runtime — produces the **same** [`SandboxReport`]. A harness that
//! cannot produce that report cannot pass an audit.
//!
//! Two rules make the difference between a security feature and security theatre:
//!
//! 1. **Fail closed.** If the requested policy cannot be enforced, [`SandboxBackend::spawn`]
//!    returns an error naming the missing control. There is no quiet best-effort mode.
//! 2. **Report what was enforced, not what was asked for.** [`SandboxReport::enforced`] is
//!    populated from what the platform actually did. Landlock's ABI level in particular must be
//!    detected at runtime — it is not safe to assume, since long-lived enterprise distributions
//!    ship kernels without it and the LSM must additionally be enabled at boot.

// Trait methods here return `impl Future<..> + Send` rather than using `async fn`. The reason is
// concrete rather than stylistic: `async fn` in a trait leaves the future's auto traits unnameable,
// `dynosaur`'s erasure then boxes it as a plain `dyn Future`, and the agent loop cannot spawn the
// result. Writing the bound out fixes that and states the requirement in the public API.
// `provider::tests::erased_provider_futures_are_send` holds the line.
use std::future::Future;
use std::path::PathBuf;

use smol_str::SmolStr;

use crate::capability::{HostPattern, PathScope, ProgramScope};

/// Which backend enforced a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackendId {
    /// Linux: Landlock, seccomp-bpf, and a user-notification supervisor.
    Landlock,
    /// Linux without Landlock: user namespaces, seccomp, and rlimits. Fewer guarantees.
    LinuxNamespaces,
    /// macOS Seatbelt.
    Seatbelt,
    /// Windows AppContainer with a job object.
    WindowsAppContainer,
    /// Windows low-integrity restricted token with a job object. Works without elevation.
    WindowsRestrictedToken,
    /// A WebAssembly runtime, for plugin code compiled to Wasm.
    Wasm,
}

/// One thing a sandbox can enforce.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Control {
    /// Reads outside the granted scope are denied.
    FilesystemRead,
    /// Writes outside the granted scope are denied.
    FilesystemWrite,
    /// Outbound connections to hosts off the allowlist are denied.
    NetworkEgress,
    /// Only allowlisted programs may be executed.
    ProgramAllowlist,
    /// Memory is capped.
    MemoryLimit,
    /// CPU time is capped.
    CpuLimit,
    /// Process count is capped.
    ProcessLimit,
    /// Wall-clock time is capped.
    WallClockLimit,
    /// Writes are captured so they can be reviewed and reverted.
    CopyOnWrite,
}

/// Which controls a backend actually applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EnforcedSet(Vec<Control>);

impl EnforcedSet {
    /// A set containing `controls`.
    pub fn new(controls: impl IntoIterator<Item = Control>) -> Self {
        let mut v: Vec<Control> = controls.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Self(v)
    }

    /// Whether a control was applied.
    #[must_use]
    pub fn has(&self, control: Control) -> bool {
        self.0.contains(&control)
    }

    /// Which of `wanted` were not applied.
    #[must_use]
    pub fn missing_from(&self, wanted: &[Control]) -> Vec<Control> {
        wanted.iter().copied().filter(|c| !self.has(*c)).collect()
    }

    /// The controls, sorted.
    #[must_use]
    pub fn controls(&self) -> &[Control] {
        &self.0
    }
}

/// A control that was requested but could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Degradation {
    /// What could not be enforced.
    pub control: Control,
    /// Why, in terms an operator can act on.
    pub reason: String,
}

/// Something the sandboxed process did to the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FsEffect {
    /// A file was created.
    Created {
        /// Which path.
        path: PathBuf,
        /// How many bytes.
        bytes: u64,
    },
    /// A file was modified.
    Modified {
        /// Which path.
        path: PathBuf,
        /// How many bytes it now has.
        bytes: u64,
    },
    /// A file was removed.
    Removed {
        /// Which path.
        path: PathBuf,
    },
}

/// An outbound connection the process attempted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EgressAttempt {
    /// The host it tried to reach.
    pub host: String,
    /// Whether it was permitted.
    pub allowed: bool,
}

/// A limit the process ran into.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LimitHit {
    /// Which limit.
    pub control: Control,
    /// What the limit was.
    pub limit: u64,
}

/// Resource ceilings for a sandboxed process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    /// Wall-clock milliseconds.
    pub wall_ms: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Maximum concurrent processes.
    pub max_pids: u32,
    /// Maximum bytes of captured output. Beyond this, output is truncated — and the truncation is
    /// reported, never silent.
    pub max_output_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_ms: 120_000,
            memory_bytes: 2 * 1024 * 1024 * 1024,
            max_pids: 64,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }
}

/// What a sandboxed process is permitted to do.
///
/// Everything is deny-by-default: an empty policy grants nothing at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxPolicy {
    /// Readable paths.
    pub fs_read: PathScope,
    /// Writable paths.
    pub fs_write: PathScope,
    /// Reachable hosts. Concrete names only, resolved once when the sandbox starts, which is what
    /// closes DNS rebinding.
    pub egress: Vec<HostPattern>,
    /// Executable programs.
    pub exec: ProgramScope,
    /// Resource ceilings.
    pub limits: Limits,
    /// Whether writes should be captured to an overlay so they can be reviewed and reverted.
    pub copy_on_write: bool,
}

impl SandboxPolicy {
    /// The controls this policy needs in order to be honestly enforced.
    #[must_use]
    pub fn required_controls(&self) -> Vec<Control> {
        let mut wanted = vec![
            Control::FilesystemRead,
            Control::FilesystemWrite,
            Control::NetworkEgress,
            Control::MemoryLimit,
            Control::WallClockLimit,
            Control::ProcessLimit,
        ];
        if !self.exec.programs().is_empty() {
            wanted.push(Control::ProgramAllowlist);
        }
        if self.copy_on_write {
            wanted.push(Control::CopyOnWrite);
        }
        wanted.sort_unstable();
        wanted
    }
}

/// A program to run under a sandbox.
///
/// `argv`, never a command string. There is deliberately no field that would let a caller pass
/// something for a shell to parse, and the environment starts empty rather than inherited, so an
/// API key cannot leak into a subprocess by default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecSpec {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Working directory. Must fall inside the policy's write scope.
    pub cwd: PathBuf,
    /// Environment variables, by name and value. Empty by default.
    pub env: Vec<(SmolStr, String)>,
}

impl ExecSpec {
    /// A program with no environment.
    pub fn new(argv: impl IntoIterator<Item = impl Into<String>>, cwd: impl Into<PathBuf>) -> Self {
        Self { argv: argv.into_iter().map(Into::into).collect(), cwd: cwd.into(), env: Vec::new() }
    }

    /// The program name, which is what the exec allowlist compares against — as a whole argv
    /// element, never as a substring of a rendered command line.
    #[must_use]
    pub fn program(&self) -> Option<&str> {
        self.argv.first().map(String::as_str)
    }
}

/// Everything that happened inside a sandbox. The audit artefact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxReport {
    /// Which backend ran it.
    pub backend: BackendId,
    /// What was **actually** enforced.
    pub enforced: EnforcedSet,
    /// What was asked for and could not be enforced.
    pub degraded: Vec<Degradation>,
    /// What the process did to the filesystem.
    pub fs_effects: Vec<FsEffect>,
    /// Where it tried to connect, allowed and denied alike. A denied attempt is a signal, not noise.
    pub egress_attempts: Vec<EgressAttempt>,
    /// Limits it ran into.
    pub limits_hit: Vec<LimitHit>,
    /// Process exit code, if it exited normally.
    pub exit_code: Option<i32>,
    /// How many bytes of output were withheld.
    pub output_bytes_elided: u64,
}

impl SandboxReport {
    /// Whether the run was fully confined as requested.
    #[must_use]
    pub fn is_fully_enforced(&self) -> bool {
        self.degraded.is_empty()
    }

    /// Hosts the process tried and failed to reach. The most interesting line in an audit.
    #[must_use]
    pub fn denied_hosts(&self) -> Vec<&str> {
        self.egress_attempts.iter().filter(|a| !a.allowed).map(|a| a.host.as_str()).collect()
    }
}

/// What a backend can enforce on this host, discovered at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    /// Whether the backend can run here at all.
    pub usable: bool,
    /// What it can enforce here. On Linux this depends on the kernel's Landlock ABI level and on
    /// whether the LSM was enabled at boot, so it must be probed rather than assumed.
    pub controls: EnforcedSet,
    /// A human-readable description, e.g. `landlock ABI 6` or `landlock unavailable: not in lsm=`.
    pub detail: String,
}

impl Availability {
    /// A backend that cannot run here.
    pub fn unusable(detail: impl Into<String>) -> Self {
        Self { usable: false, controls: EnforcedSet::default(), detail: detail.into() }
    }
}

/// A sandbox could not be created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// No backend on this host can enforce the requested policy.
    ///
    /// This is the fail-closed path, and it is an error rather than a warning on purpose: running
    /// unconfined because confinement was unavailable is the failure mode that makes an audit go
    /// badly.
    #[error(
        "cannot enforce {missing:?} on this host ({detail}); refusing to run unconfined. \
         Set security.allow_degraded_sandbox to accept reduced confinement deliberately."
    )]
    CannotEnforce {
        /// Which controls are unavailable.
        missing: Vec<Control>,
        /// What was probed.
        detail: String,
    },
    /// The policy itself was invalid, e.g. a working directory outside the write scope.
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    /// The process could not be started.
    #[error("could not start `{program}`: {detail}")]
    SpawnFailed {
        /// Which program.
        program: String,
        /// Why.
        detail: String,
    },
    /// The run was cancelled.
    #[error("cancelled")]
    Cancelled,
}

/// A finished sandboxed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandboxed {
    /// Captured standard output, truncated to the policy's limit.
    pub stdout: Vec<u8>,
    /// Captured standard error, truncated to the policy's limit.
    pub stderr: Vec<u8>,
    /// The audit artefact.
    pub report: SandboxReport,
}

/// A platform mechanism for confining a process.
#[dynosaur::dynosaur(pub DynSandboxBackend = dyn(box) SandboxBackend)]
pub trait SandboxBackend: Send + Sync {
    /// Which backend this is.
    fn id(&self) -> BackendId;

    /// What this backend can enforce on this host, right now.
    fn probe(&self) -> Availability;

    /// Run `spec` under `policy`.
    ///
    /// # Errors
    /// Returns [`SandboxError::CannotEnforce`] rather than running with weaker confinement than was
    /// asked for.
    fn run(
        &self,
        spec: ExecSpec,
        policy: &SandboxPolicy,
    ) -> impl Future<Output = Result<Sandboxed, SandboxError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: PathScope::new(["./"]).unwrap(),
            fs_write: PathScope::new(["./out"]).unwrap(),
            egress: vec![HostPattern::new("api.github.com").unwrap()],
            exec: ProgramScope::new(["git"]),
            limits: Limits::default(),
            copy_on_write: true,
        }
    }

    #[test]
    fn an_empty_policy_grants_nothing() {
        let p = SandboxPolicy::default();
        assert!(p.fs_read.is_empty());
        assert!(p.fs_write.is_empty());
        assert!(p.egress.is_empty());
        assert!(p.exec.programs().is_empty());
    }

    #[test]
    fn required_controls_follow_from_the_policy() {
        let required = policy().required_controls();
        assert!(required.contains(&Control::ProgramAllowlist), "exec was requested");
        assert!(required.contains(&Control::CopyOnWrite), "copy-on-write was requested");

        let no_exec = SandboxPolicy { exec: ProgramScope::default(), ..policy() };
        assert!(!no_exec.required_controls().contains(&Control::ProgramAllowlist));
    }

    #[test]
    fn a_backend_that_cannot_enforce_the_policy_names_exactly_what_is_missing() {
        // What a Linux host without Landlock looks like: rlimits work, filesystem scoping does not.
        let enforced = EnforcedSet::new([
            Control::MemoryLimit,
            Control::WallClockLimit,
            Control::ProcessLimit,
        ]);
        let missing = enforced.missing_from(&policy().required_controls());

        assert!(missing.contains(&Control::FilesystemRead));
        assert!(missing.contains(&Control::NetworkEgress));

        let err = SandboxError::CannotEnforce {
            missing,
            detail: "landlock unavailable: kernel 5.14, ABI 0".into(),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("FilesystemRead"), "the operator must learn what is missing");
        assert!(rendered.contains("refusing to run unconfined"));
        assert!(rendered.contains("allow_degraded_sandbox"), "and how to proceed deliberately");
    }

    #[test]
    fn a_report_distinguishes_what_was_enforced_from_what_was_asked() {
        let report = SandboxReport {
            backend: BackendId::LinuxNamespaces,
            enforced: EnforcedSet::new([Control::FilesystemRead, Control::MemoryLimit]),
            degraded: vec![Degradation {
                control: Control::NetworkEgress,
                reason: "no Landlock TCP scoping below ABI 4".into(),
            }],
            fs_effects: vec![FsEffect::Created { path: "out/a.txt".into(), bytes: 12 }],
            egress_attempts: vec![
                EgressAttempt { host: "api.github.com".into(), allowed: true },
                EgressAttempt { host: "evil.test".into(), allowed: false },
            ],
            limits_hit: Vec::new(),
            exit_code: Some(0),
            output_bytes_elided: 0,
        };

        assert!(!report.is_fully_enforced(), "a degraded run must never look like a clean one");
        assert_eq!(report.denied_hosts(), ["evil.test"]);

        let decoded: SandboxReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(decoded, report, "the audit artefact must survive being written down");
    }

    #[test]
    fn exec_specs_carry_argv_not_a_command_string() {
        let spec = ExecSpec::new(["git", "status", "--porcelain"], "./");
        assert_eq!(spec.program(), Some("git"));
        assert!(spec.env.is_empty(), "the environment starts empty, not inherited");

        // The obfuscations that defeat regex denylists have nothing to work on: the program is
        // compared as a whole argv element.
        let sneaky = ExecSpec::new(["sh", "-c", "r''m -rf /"], "./");
        assert_eq!(sneaky.program(), Some("sh"));
        assert!(!ProgramScope::new(["git", "cargo"]).covers(sneaky.program().unwrap()));
    }

    #[test]
    fn backends_are_object_safe_through_dynosaur() {
        struct Stub;
        impl SandboxBackend for Stub {
            fn id(&self) -> BackendId {
                BackendId::Wasm
            }
            fn probe(&self) -> Availability {
                Availability::unusable("stub")
            }
            async fn run(
                &self,
                _spec: ExecSpec,
                _policy: &SandboxPolicy,
            ) -> Result<Sandboxed, SandboxError> {
                Err(SandboxError::Cancelled)
            }
        }
        let erased: Box<DynSandboxBackend<'static>> = DynSandboxBackend::new_box(Stub);
        assert_eq!(erased.id(), BackendId::Wasm);
        assert!(!erased.probe().usable);
    }
}
