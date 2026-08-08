//! ADR-0011 prototype: is `Tainted<T, I, C>` ergonomic enough for real tools?
//!
//! The decision criterion recorded in `notes/adr/decision-log.md` is:
//!
//! > Can a competent Rust developer write a new tool without ever mentioning a label?
//!
//! This file answers it by writing the three tools the ADR named — `fs_read`, `http_get`, and
//! `shell` — plus the two harder cases: a tool that *consumes* another tool's output, and a sink
//! that must refuse untrusted data. Each section states what it is evidence for.

use frey_core::audit::{AuditEvent, Endorsement, MemorySink, scoped_sink};
use frey_core::error::{ToolError, ToolErrorKind, ToolOutcome};
use frey_core::taint::{Provenance, Tainted, Trusted, Untrusted, Validated};
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// The three tools from the ADR. Note what is absent: no label, no `Tainted`, no `endorse`.
// ---------------------------------------------------------------------------------------------

/// A path that has been checked to lie inside the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspacePath(String);

/// A URL that parsed and whose host is on the egress allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AllowedUrl(String);

/// An argument vector. Never a shell command string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellArgv(Vec<String>);

fn fs_read(path: &WorkspacePath) -> Result<String, ToolError> {
    if path.0 == "src/main.rs" {
        Ok("fn main() {}".to_string())
    } else {
        Err(ToolError::new(ToolErrorKind::NotFound, format!("no file at {}", path.0))
            .guide("List the directory with `fs_list` before reading.")
            .suggest(["fs_list"]))
    }
}

fn http_get(url: &AllowedUrl) -> Result<String, ToolError> {
    Ok(format!("<html>body of {}</html>", url.0))
}

fn shell(argv: &ShellArgv) -> Result<String, ToolError> {
    Ok(format!("ran {:?}", argv.0))
}

// ---------------------------------------------------------------------------------------------
// Validators. A tool author writes these once per argument type, not once per tool.
// ---------------------------------------------------------------------------------------------

struct InWorkspace;
impl Validated<String> for InWorkspace {
    type Output = WorkspacePath;
    type Error = &'static str;
    const NAME: &'static str = "InWorkspace";

    fn validate(raw: String) -> Result<WorkspacePath, &'static str> {
        if raw.starts_with('/') || raw.contains("..") {
            Err("path escapes the workspace")
        } else {
            Ok(WorkspacePath(raw))
        }
    }
}

struct OnEgressAllowlist;
impl Validated<String> for OnEgressAllowlist {
    type Output = AllowedUrl;
    type Error = &'static str;
    const NAME: &'static str = "OnEgressAllowlist";

    fn validate(raw: String) -> Result<AllowedUrl, &'static str> {
        const ALLOWED: &[&str] = &["api.github.com", "crates.io"];
        let host = raw.strip_prefix("https://").and_then(|r| r.split('/').next()).unwrap_or("");
        if ALLOWED.contains(&host) {
            Ok(AllowedUrl(raw))
        } else {
            Err("host is not on the allowlist")
        }
    }
}

struct ParsedArgv;
impl Validated<Vec<String>> for ParsedArgv {
    type Output = ShellArgv;
    type Error = &'static str;
    const NAME: &'static str = "ParsedArgv";

    fn validate(raw: Vec<String>) -> Result<ShellArgv, &'static str> {
        const ALLOWED_PROGRAMS: &[&str] = &["git", "cargo", "rg"];
        match raw.first() {
            None => Err("empty argv"),
            Some(p) if ALLOWED_PROGRAMS.contains(&p.as_str()) => Ok(ShellArgv(raw)),
            Some(_) => Err("program is not on the exec allowlist"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The boundary. Written once, in the framework, not by tool authors.
// ---------------------------------------------------------------------------------------------

/// Everything a tool returns is untrusted, because a tool reads the outside world.
fn invoke<T>(tool: &str, result: Result<T, ToolError>) -> ToolOutcome<Untrusted<T>> {
    match result {
        Ok(v) => ToolOutcome::Ok(Tainted::from_tool(tool, v)),
        Err(e) => ToolOutcome::Failed(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------------------------

/// Evidence for the ADR criterion: the model proposes a raw string, the argument *type* forces the
/// check, and the tool body never mentions a label.
#[test]
fn a_tool_author_never_writes_a_label() {
    let sink = Arc::new(MemorySink::new());
    let _guard = scoped_sink(sink.clone());

    // What the model actually gives us: an unvalidated string, low integrity by construction.
    let proposed: Untrusted<String> = Tainted::from_model("claude-opus-5", "src/main.rs".into());

    // Validation narrows the type and raises integrity in one move.
    let path: Trusted<WorkspacePath> = proposed.validate::<InWorkspace>().expect("path is inside");

    let outcome = invoke("fs_read", fs_read(path.peek()));
    let ToolOutcome::Ok(contents) = outcome else { panic!("expected success") };

    // The result is untrusted again — a file's contents are attacker-controlled.
    assert_eq!(contents.label().0, frey_core::taint::IntegrityLevel::Low);
    assert_eq!(contents.provenance().origin.as_str(), "tool:fs_read");

    // Exactly one endorsement, and it names the validator rather than a human.
    match sink.events().as_slice() {
        [AuditEvent::Endorsed { reason: Endorsement::Parsed { validator }, .. }] => {
            assert_eq!(*validator, "InWorkspace");
        }
        other => panic!("unexpected audit trail: {other:?}"),
    }
}

/// Evidence that the failure path is a normal, informative tool error rather than a panic or a
/// silent denial — and that a rejected value can still be traced to whatever produced it.
#[test]
fn rejected_arguments_are_traceable_to_their_source() {
    let escape: Untrusted<String> = Tainted::from_model("claude-opus-5", "../../etc/passwd".into());
    let err = escape.validate::<InWorkspace>().expect_err("traversal is rejected");
    assert_eq!(err.validator, "InWorkspace");
    assert_eq!(err.provenance.origin.as_str(), "model:claude-opus-5");
    assert!(format!("{err}").contains("escapes the workspace"));
}

/// The egress case: a URL that came from a *fetched page* (not from the operator) still has to
/// clear the allowlist before it can be used, and the provenance shows the whole chain.
#[test]
fn indirect_prompt_injection_still_has_to_clear_the_allowlist() {
    // A page we fetched contains a link. This is the classic injection vector.
    let page: Untrusted<String> = Tainted::from_tool("http_get", "https://evil.test/exfil".into());
    let attempt = page.clone().validate::<OnEgressAllowlist>();
    assert!(attempt.is_err(), "a host off the allowlist must not become callable");

    // The same mechanism permits the legitimate case.
    let good: Untrusted<String> =
        Tainted::from_tool("http_get", "https://api.github.com/repos".into());
    let url = good.through("link-extractor").validate::<OnEgressAllowlist>().expect("allowlisted");
    assert!(url.provenance().summary().contains("link-extractor"));

    let ToolOutcome::Ok(body) = invoke("http_get", http_get(url.peek())) else {
        panic!("expected success")
    };
    assert!(body.peek().contains("api.github.com"));
}

/// The shell case: `argv`, never a command string, and the allowlist is checked by a parser rather
/// than by a regex over a rendered command line.
#[test]
fn shell_takes_argv_and_the_program_allowlist_is_a_parser() {
    let proposed: Untrusted<Vec<String>> =
        Tainted::from_model("claude-opus-5", vec!["git".into(), "status".into()]);
    let argv = proposed.validate::<ParsedArgv>().expect("git is allowed");
    let ToolOutcome::Ok(out) = invoke("shell", shell(argv.peek())) else { panic!("expected ok") };
    assert!(out.peek().contains("git"));

    // The obfuscations that defeat regex denylists are irrelevant here: there is no string to
    // obfuscate. The program name is compared as a whole argv element.
    let sneaky: Untrusted<Vec<String>> =
        Tainted::from_model("claude-opus-5", vec!["sh".into(), "-c".into(), "r''m -rf / #".into()]);
    assert!(sneaky.validate::<ParsedArgv>().is_err(), "sh is not on the exec allowlist");
}

/// The harder case the ADR worried about: a tool that genuinely consumes untrusted content.
/// It *does* mention a label — and that is the correct outcome, because its signature is now
/// self-documenting about what it accepts.
#[test]
fn a_tool_that_consumes_untrusted_content_says_so_in_its_signature() {
    fn summarise(text: &Untrusted<String>) -> Result<String, ToolError> {
        Ok(format!("{} chars from {}", text.peek().len(), text.provenance().summary()))
    }

    let page: Untrusted<String> = Tainted::from_tool("http_get", "0123456789".into());
    let summary = summarise(&page).expect("summarising is always allowed");
    assert!(summary.starts_with("10 chars from tool:http_get"));
}

/// A sink that changes the world requires high integrity and public confidentiality. This is the
/// property that makes the type parameters worth their weight: the precondition is checked by the
/// compiler at every call site, not by a reviewer reading the body.
#[test]
fn side_effecting_sinks_consume_only_trusted_public_values() {
    fn commit(action: Trusted<ShellArgv>) -> String {
        format!("executed {:?}", action.into_inner().0)
    }

    let argv: Untrusted<Vec<String>> =
        Tainted::from_model("claude-opus-5", vec!["cargo".into(), "test".into()]);
    let checked = argv.validate::<ParsedArgv>().expect("cargo is allowed");
    assert!(commit(checked).contains("cargo"));

    // The negative case is a compile error, not a runtime check. See `tests/ui/`.
}

/// Secrets travel with the value and cannot be released implicitly, even when the value is fully
/// trusted. Integrity and confidentiality really are independent axes.
#[test]
fn trusted_does_not_imply_releasable() {
    let sink = Arc::new(MemorySink::new());
    let _guard = scoped_sink(sink.clone());

    let token: Tainted<String, frey_core::taint::High, frey_core::taint::Secret> =
        Tainted::with_provenance("ghp_realtoken".into(), Provenance::new("secret:github_token"));

    // Mixing a secret into anything makes the result secret.
    let request: Trusted<String> = Tainted::from_operator("template", "GET /repos".into());
    let combined = request.zip(token);
    assert_eq!(combined.label().1, frey_core::taint::ConfidentialityLevel::Secret);

    // Releasing it is a deliberate, audited act.
    let released = combined.declassify(frey_core::audit::Declassification::Redacted {
        redactor: "strip_authorization_header",
    });
    assert_eq!(released.label().1, frey_core::taint::ConfidentialityLevel::Public);
    assert!(matches!(sink.events().as_slice(), [AuditEvent::Declassified { .. }]));
}
