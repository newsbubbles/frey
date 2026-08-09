//! The tools that ship in the box.
//!
//! Each one exists partly to be useful and partly to demonstrate the shape a safe tool takes. The
//! shell tool is the important one: almost every framework's version interpolates model output into
//! a command string, inherits the parent's environment, and calls a regex denylist of `rm -rf`
//! "safety". All three are wrong, and the tests here say so concretely.
//!
//! Note what these tools *do not* do. They validate and describe; they do not touch the filesystem
//! or the network themselves. Execution goes through `frey-sandbox`, which is the only place a
//! process is spawned, so there is one place to audit rather than one per tool.

use frey_core::capability::{HostPattern, PathScope, ProgramScope};
use frey_core::error::{ToolError, ToolErrorKind};
use frey_core::taint::{Tainted, Untrusted, Validated};

/// A path that has been checked to lie inside the workspace.
///
/// The check is lexical and deliberately conservative; the sandbox re-checks with the kernel. This
/// exists so a traversal attempt is rejected with an explanation the model can act on, rather than
/// as a permission error from three layers down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// The path, relative to the workspace root.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validates a path against the workspace scope.
#[derive(Debug, Clone)]
pub struct InWorkspace {
    scope: PathScope,
}

impl InWorkspace {
    /// A validator for `scope`.
    #[must_use]
    pub fn new(scope: PathScope) -> Self {
        Self { scope }
    }

    /// Check `raw`.
    ///
    /// # Errors
    /// Returns a message naming the problem, so the model can correct itself rather than retrying.
    pub fn check(&self, raw: &str) -> Result<WorkspacePath, &'static str> {
        if raw.is_empty() {
            return Err("the path is empty");
        }
        if raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':') {
            return Err("absolute paths are outside the workspace");
        }
        if raw.split(['/', '\\']).any(|c| c == "..") {
            return Err("`..` would leave the workspace");
        }
        if !self.scope.covers(raw) {
            return Err("that path is outside the granted scope");
        }
        Ok(WorkspacePath(raw.to_string()))
    }
}

/// A URL whose host is on the egress allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedUrl {
    /// The full URL.
    pub url: String,
    /// The host, lowercased.
    pub host: String,
}

/// Validates a URL against the egress allowlist.
#[derive(Debug, Clone)]
pub struct OnEgressAllowlist {
    allowed: Vec<HostPattern>,
}

impl OnEgressAllowlist {
    /// A validator permitting `allowed`.
    #[must_use]
    pub fn new(allowed: Vec<HostPattern>) -> Self {
        Self { allowed }
    }

    /// Check `raw`.
    ///
    /// # Errors
    /// Returns a message naming the host, so an operator reading the log can decide whether to
    /// widen the allowlist.
    pub fn check(&self, raw: &str) -> Result<AllowedUrl, String> {
        let rest = raw
            .strip_prefix("https://")
            .ok_or_else(|| format!("`{raw}` is not an https URL; plaintext egress is refused"))?;
        let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
        // Reject embedded credentials and ports: both are ways to reach somewhere other than the
        // host that appears to be named.
        if host.contains('@') || host.contains(':') {
            return Err(format!("`{host}` embeds credentials or a port, which is refused"));
        }
        if self.allowed.iter().any(|h| h.as_str() == host) {
            Ok(AllowedUrl { url: raw.to_string(), host })
        } else {
            Err(format!(
                "`{host}` is not on the egress allowlist ({})",
                self.allowed.iter().map(HostPattern::as_str).collect::<Vec<_>>().join(", ")
            ))
        }
    }
}

/// An argument vector whose program is on the exec allowlist.
///
/// **Never a command string.** There is no constructor taking one, because the moment a tool
/// accepts a string for a shell to parse, quoting bugs and obfuscation become the caller's problem
/// and a denylist becomes the only available defence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellArgv(Vec<String>);

impl ShellArgv {
    /// The argument vector.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// The program.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.0[0]
    }
}

/// Validates an argument vector against the exec allowlist.
#[derive(Debug, Clone)]
pub struct AllowedProgram {
    scope: ProgramScope,
}

impl AllowedProgram {
    /// A validator for `scope`.
    #[must_use]
    pub fn new(scope: ProgramScope) -> Self {
        Self { scope }
    }

    /// Check `argv`.
    ///
    /// # Errors
    /// Returns a message naming what is permitted.
    pub fn check(&self, argv: Vec<String>) -> Result<ShellArgv, String> {
        let Some(program) = argv.first() else {
            return Err("the argument vector is empty".to_string());
        };
        if !self.scope.covers(program) {
            return Err(format!(
                "`{program}` is not on the exec allowlist ({})",
                self.scope.programs().join(", ")
            ));
        }
        Ok(ShellArgv(argv))
    }
}

/// A validated JSON document of a known shape.
///
/// Narrowing a type *is* the check, which is why this raises integrity: the parser, not the model,
/// decided the shape.
pub struct ParsedJson<T>(std::marker::PhantomData<T>);

impl<T: serde::de::DeserializeOwned> Validated<String> for ParsedJson<T> {
    type Output = T;
    type Error = String;
    const NAME: &'static str = "ParsedJson";

    fn validate(raw: String) -> Result<T, String> {
        serde_json::from_str(&raw).map_err(|e| e.to_string())
    }
}

/// Turn a validation failure into an error the model can act on.
///
/// A bare "denied" teaches the model nothing and produces a retry loop with the same arguments,
/// which is why every rejection here carries guidance.
#[must_use]
pub fn rejection(tool: &str, reason: impl std::fmt::Display, guidance: &str) -> ToolError {
    ToolError::new(ToolErrorKind::Denied, format!("`{tool}` refused those arguments: {reason}"))
        .guide(guidance.to_string())
}

/// Label a tool's output. Applied at the boundary so tool authors never write a label themselves.
#[must_use]
pub fn label_output(tool: &str, text: String) -> Untrusted<String> {
    Tainted::from_tool(tool, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> InWorkspace {
        InWorkspace::new(PathScope::new(["./src", "./out"]).unwrap())
    }

    fn egress() -> OnEgressAllowlist {
        OnEgressAllowlist::new(vec![
            HostPattern::new("api.github.com").unwrap(),
            HostPattern::new("crates.io").unwrap(),
        ])
    }

    fn programs() -> AllowedProgram {
        AllowedProgram::new(ProgramScope::new(["git", "cargo", "rg"]))
    }

    #[test]
    fn traversal_is_rejected_in_several_disguises() {
        for attempt in ["../etc/passwd", "src/../../etc", "/etc/passwd", "C:\\Windows"] {
            assert!(workspace().check(attempt).is_err(), "{attempt} must be refused");
        }
        assert!(workspace().check("src/main.rs").is_ok());
    }

    #[test]
    fn a_path_outside_the_granted_scope_is_refused_even_without_traversal() {
        assert!(workspace().check("secrets/keys.txt").is_err());
        assert!(workspace().check("out/report.md").is_ok());
    }

    #[test]
    fn a_rejection_tells_the_model_what_is_wrong() {
        let reason = workspace().check("../etc").unwrap_err();
        assert!(reason.contains("leave the workspace"), "{reason}");
    }

    #[test]
    fn egress_is_refused_for_anything_off_the_allowlist() {
        let err = egress().check("https://evil.test/exfil").unwrap_err();
        assert!(err.contains("evil.test"), "name the host: {err}");
        assert!(err.contains("api.github.com"), "and what is permitted: {err}");
    }

    #[test]
    fn credentials_and_ports_in_a_url_are_refused() {
        // `https://api.github.com@evil.test/` reads as GitHub and resolves to evil.test.
        assert!(egress().check("https://api.github.com@evil.test/x").is_err());
        assert!(egress().check("https://api.github.com:8080/x").is_err());
    }

    #[test]
    fn plaintext_egress_is_refused() {
        assert!(egress().check("http://api.github.com/x").is_err());
        assert!(egress().check("https://api.github.com/repos").is_ok());
    }

    #[test]
    fn the_allowlisted_case_still_works() {
        let url = egress().check("https://crates.io/api/v1/crates/frey").unwrap();
        assert_eq!(url.host, "crates.io");
    }

    #[test]
    fn a_program_is_compared_as_a_whole_argv_element() {
        // The reason the tool takes argv: the obfuscations that defeat a regex denylist have
        // nothing to work on. There is no command string to mangle.
        for attempt in [
            vec!["sh".to_string(), "-c".into(), "r''m -rf /".into()],
            vec!["/bin/sh".to_string()],
            vec!["git; rm -rf /".to_string()],
        ] {
            assert!(programs().check(attempt.clone()).is_err(), "{attempt:?} must be refused");
        }

        let ok =
            programs().check(vec!["git".into(), "status".into(), "--porcelain".into()]).unwrap();
        assert_eq!(ok.program(), "git");
        assert_eq!(ok.as_slice().len(), 3);
    }

    #[test]
    fn an_empty_argv_is_refused_rather_than_panicking() {
        assert!(programs().check(Vec::new()).is_err());
    }

    #[test]
    fn parsing_raises_integrity_because_the_parser_decided_the_shape() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Report {
            ok: bool,
        }
        let raw: Untrusted<String> = Tainted::from_tool("http_get", r#"{"ok":true}"#.into());
        let parsed = raw.validate::<ParsedJson<Report>>().unwrap();
        assert_eq!(parsed.label().0, frey_core::taint::IntegrityLevel::High);
        assert_eq!(parsed.into_inner(), Report { ok: true });
    }

    #[test]
    fn malformed_json_fails_with_the_source_recorded() {
        #[derive(serde::Deserialize, Debug)]
        struct Report {
            #[allow(dead_code)]
            ok: bool,
        }
        let raw: Untrusted<String> = Tainted::from_peer("scraper", "not json".into());
        let err = raw.validate::<ParsedJson<Report>>().unwrap_err();
        assert_eq!(err.provenance.origin.as_str(), "peer:scraper");
    }

    #[test]
    fn tool_output_is_untrusted_and_carries_its_origin() {
        let out = label_output("shell", "command output".into());
        assert_eq!(out.label().0, frey_core::taint::IntegrityLevel::Low);
        assert_eq!(out.provenance().origin.as_str(), "tool:shell");
    }

    #[test]
    fn a_rejection_error_carries_guidance() {
        let err = rejection("shell", "curl is not allowed", "Use `http_get` instead.");
        assert_eq!(err.kind(), ToolErrorKind::Denied);
        assert!(err.model().guidance.as_deref().unwrap().contains("http_get"));
    }
}
