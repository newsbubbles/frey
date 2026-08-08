//! The layers every tool call passes through.
//!
//! `pydantic-ai` needs twelve `*Toolset` classes to express filtering, renaming, prefixing,
//! preparation, approval, deferral, metadata and wrapping. All twelve are middleware over one
//! operation, which Rust already has a vocabulary for — so here they are one chain instead
//! (ADR-0005).
//!
//! The important consequence is not brevity. It is that **there is no second path to executing a
//! tool**. Native functions, MCP tools, skill scripts, sub-agents and remote peers all arrive here,
//! so policy, approval, redaction and audit are unavoidable rather than opt-in.

use std::sync::{Arc, Mutex};

use frey_core::capability::Capability;
use frey_core::error::{InputRequest, NeedsInput, Risk, ToolError, ToolErrorKind, ToolOutcome};
use frey_core::ids::ToolName;
use frey_core::tool::{Invocation, ToolCx, ToolValue};
use frey_core::tool_def::{CallerPolicy, CostHint, ToolDefinition};

/// What a layer wraps: one tool invocation.
pub trait ToolService: Send + Sync {
    /// The definition of whatever sits at the bottom of the stack.
    fn definition(&self) -> &ToolDefinition;

    /// Run the call.
    fn call(
        &self,
        invocation: Invocation,
        cx: &ToolCx,
    ) -> impl Future<Output = ToolOutcome<ToolValue>> + Send;
}

/// Decides whether a call is permitted at all.
///
/// Three checks, in order of how cheaply they fail: the caller policy, then capabilities, then
/// argument validity. A denial always tells the model what to do instead, because a denial it
/// cannot act on just produces a retry loop.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyLayer;

impl PolicyLayer {
    /// Check `invocation` against `definition` and the grants in `cx`.
    ///
    /// Returns `None` when the call may proceed.
    #[must_use]
    pub fn check(
        definition: &ToolDefinition,
        invocation: &Invocation,
        cx: &ToolCx,
    ) -> Option<ToolError> {
        // Anthropic document `allowed_callers` as guidance to the model rather than a boundary, so
        // Frey enforces it here. Without this the "code-only" marking is decoration.
        let allowed = if invocation.caller.is_code() {
            definition.caller.allows_code()
        } else {
            definition.caller.allows_direct()
        };
        if !allowed {
            return Some(
                ToolError::new(
                    ToolErrorKind::Denied,
                    format!("`{}` cannot be called from there", definition.name),
                )
                .guide(match definition.caller {
                    CallerPolicy::CodeOnly => "Call this tool from inside a code block instead.",
                    _ => "Call this tool directly rather than from code.",
                }),
            );
        }

        if let Some(missing) = missing_capability(definition, cx) {
            return Some(
                ToolError::new(
                    ToolErrorKind::Denied,
                    format!("`{}` needs a capability this agent was not granted", definition.name),
                )
                .guide(format!(
                    "The missing grant is {missing}. Ask the operator to widen it, or use a tool \
                     that works within the current grants."
                )),
            );
        }

        None
    }
}

fn missing_capability(definition: &ToolDefinition, cx: &ToolCx) -> Option<String> {
    definition.capabilities.iter().find(|c| !cx.grants.permits(c)).map(describe_capability)
}

fn describe_capability(capability: &Capability) -> String {
    match capability {
        Capability::FsRead(scope) => format!("fs:read({})", scope.prefixes().join(", ")),
        Capability::FsWrite(scope) => format!("fs:write({})", scope.prefixes().join(", ")),
        Capability::NetEgress(host) => format!("net:egress({host})"),
        Capability::Exec(scope) => format!("exec({})", scope.programs().join(", ")),
        Capability::Secret(name) => format!("secret({})", name.0),
        Capability::Spend(budget) => format!("spend({} micros)", budget.micros),
        Capability::Mcp { server, .. } => format!("mcp({server})"),
        Capability::Delegate(agent) => format!("delegate({agent})"),
        _ => "an unrecognised capability".to_string(),
    }
}

/// Decides whether a human must approve a call before it runs.
#[derive(Debug, Clone)]
pub struct ApprovalLayer {
    policy: ApprovalPolicy,
    approved: Arc<Mutex<Vec<ToolName>>>,
}

/// When approval is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalPolicy {
    /// Ask for anything at or above this risk.
    AtOrAbove(Risk),
    /// Ask for nothing. Appropriate only when the grant set is already the boundary.
    Never,
    /// Ask for everything.
    Always,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self::AtOrAbove(Risk::Medium)
    }
}

impl ApprovalLayer {
    /// A layer applying `policy`.
    #[must_use]
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self { policy, approved: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Record that a tool has been approved for the rest of the session.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    pub fn approve_for_session(&self, name: ToolName) {
        self.approved.lock().expect("approval layer poisoned").push(name);
    }

    /// Whether this call needs approval, and what to show the human if so.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    #[must_use]
    pub fn gate(&self, definition: &ToolDefinition, invocation: &Invocation) -> Option<NeedsInput> {
        if self.approved.lock().expect("approval layer poisoned").contains(&definition.name) {
            return None;
        }
        let risk = risk_of(definition);
        let needs = match self.policy {
            ApprovalPolicy::Never => false,
            ApprovalPolicy::Always => true,
            ApprovalPolicy::AtOrAbove(threshold) => risk >= threshold,
        };
        if !needs {
            return None;
        }
        Some(NeedsInput {
            token: format!("approve:{}", invocation.id).into(),
            requests: vec![InputRequest::Approval {
                // The literal action, never a summary. A natural-language rendering is exactly
                // where an injected instruction hides from the person approving it.
                literal: literal_action(definition, invocation),
                risk,
            }],
        })
    }
}

/// How dangerous a tool is, derived from what it declared — never from the model's own account of
/// what it is about to do.
#[must_use]
pub fn risk_of(definition: &ToolDefinition) -> Risk {
    if definition.cost_hint == CostHint::Destructive {
        return Risk::High;
    }
    if definition.capabilities.iter().any(Capability::is_mutating_or_egress) {
        return Risk::Medium;
    }
    Risk::Low
}

/// The exact action, rendered for a human to approve.
fn literal_action(definition: &ToolDefinition, invocation: &Invocation) -> String {
    format!("{}({})", definition.name, compact_json(&invocation.args))
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserialisable>".to_string())
}

/// Removes secrets from anything on its way to a log, a trace, or the model.
///
/// Redaction is a property of the *type* wherever possible ([`frey_core::taint`]), but tool
/// arguments arrive as free-form JSON from a model, so this catches the residue.
#[derive(Debug, Clone)]
pub struct RedactLayer {
    patterns: Vec<String>,
}

impl Default for RedactLayer {
    fn default() -> Self {
        Self {
            patterns: ["api_key", "apikey", "token", "secret", "password", "authorization"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

impl RedactLayer {
    /// Redact values whose key looks sensitive.
    #[must_use]
    pub fn redact(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let lower = k.to_ascii_lowercase();
                        if self.patterns.iter().any(|p| lower.contains(p.as_str())) {
                            (k.clone(), serde_json::Value::String("[redacted]".into()))
                        } else {
                            (k.clone(), self.redact(v))
                        }
                    })
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(|v| self.redact(v)).collect())
            }
            other => other.clone(),
        }
    }
}

/// Caps how much of a tool's output reaches the model, and says how much it withheld.
#[derive(Debug, Clone, Copy)]
pub struct TruncateLayer {
    /// Maximum bytes of output.
    pub max_bytes: usize,
}

impl Default for TruncateLayer {
    fn default() -> Self {
        Self { max_bytes: 32 * 1024 }
    }
}

impl TruncateLayer {
    /// Truncate `text`, returning it with the number of bytes withheld.
    ///
    /// Silent truncation produces bugs nobody can diagnose, so the count is returned rather than
    /// discarded, and the caller is obliged to tell the model how to get the rest.
    #[must_use]
    pub fn apply(&self, text: String) -> (String, u64) {
        if text.len() <= self.max_bytes {
            return (text, 0);
        }
        let mut cut = self.max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let elided = (text.len() - cut) as u64;
        (text[..cut].to_string(), elided)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::capability::{Capability, Grant, GrantSet, PathScope, ProgramScope};
    use frey_core::ids::{CallId, RunId, SessionId};
    use frey_core::item::Caller;
    use frey_core::taint::Provenance;
    use frey_core::tool_def::JsonSchema;

    fn cx(grants: GrantSet) -> ToolCx {
        ToolCx {
            run: RunId::new("r"),
            session: SessionId::new("s"),
            grants,
            provenance: Provenance::new("tool:t"),
        }
    }

    fn invocation(caller: Caller) -> Invocation {
        Invocation {
            id: CallId::new("c1"),
            name: ToolName::new("fs_read"),
            args: serde_json::json!({"path": "src/main.rs"}),
            caller,
        }
    }

    fn definition() -> ToolDefinition {
        ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents",
            JsonSchema::empty_object(),
        )
    }

    #[test]
    fn a_tool_without_its_capability_is_denied_and_told_which_grant_is_missing() {
        let mut def = definition();
        def.capabilities = vec![Capability::FsRead(PathScope::new(["./src"]).unwrap())];

        let err = PolicyLayer::check(&def, &invocation(Caller::Direct), &cx(GrantSet::empty()))
            .expect("must be denied");
        assert_eq!(err.kind(), ToolErrorKind::Denied);
        let guidance = err.model().guidance.clone().unwrap();
        assert!(guidance.contains("fs:read(src)"), "name the grant: {guidance}");
    }

    #[test]
    fn a_granted_tool_passes_policy() {
        let mut def = definition();
        def.capabilities = vec![Capability::FsRead(PathScope::new(["./src"]).unwrap())];
        let grants =
            GrantSet::new([Grant::operator(Capability::FsRead(PathScope::new(["./"]).unwrap()))]);
        assert!(PolicyLayer::check(&def, &invocation(Caller::Direct), &cx(grants)).is_none());
    }

    #[test]
    fn caller_policy_is_enforced_here_because_the_provider_only_suggests_it() {
        let mut def = definition();
        def.caller = CallerPolicy::CodeOnly;

        let denied = PolicyLayer::check(&def, &invocation(Caller::Direct), &cx(GrantSet::empty()))
            .expect("a direct call to a code-only tool is refused");
        assert!(denied.model().guidance.as_deref().unwrap().contains("code block"));

        let from_code = invocation(Caller::Code { runner: "srvtoolu_1".into() });
        assert!(PolicyLayer::check(&def, &from_code, &cx(GrantSet::empty())).is_none());
    }

    #[test]
    fn risk_comes_from_the_declaration_not_the_model() {
        assert_eq!(risk_of(&definition()), Risk::Low);

        let mut writes = definition();
        writes.capabilities = vec![Capability::Exec(ProgramScope::new(["git"]))];
        assert_eq!(risk_of(&writes), Risk::Medium);

        let mut destructive = definition();
        destructive.cost_hint = CostHint::Destructive;
        assert_eq!(risk_of(&destructive), Risk::High);
    }

    #[test]
    fn the_approval_prompt_shows_the_literal_action_never_a_summary() {
        // A natural-language rendering is exactly where an injected instruction hides from the
        // person approving it.
        let mut def = definition();
        def.cost_hint = CostHint::Destructive;
        let layer = ApprovalLayer::new(ApprovalPolicy::default());

        let needs = layer.gate(&def, &invocation(Caller::Direct)).expect("high risk must gate");
        let InputRequest::Approval { literal, risk } = &needs.requests[0] else {
            panic!("expected an approval request")
        };
        assert_eq!(*risk, Risk::High);
        assert_eq!(literal, r#"fs_read({"path":"src/main.rs"})"#);
    }

    #[test]
    fn low_risk_calls_are_not_gated_by_default() {
        let layer = ApprovalLayer::new(ApprovalPolicy::default());
        assert!(layer.gate(&definition(), &invocation(Caller::Direct)).is_none());
    }

    #[test]
    fn session_approval_stops_the_second_prompt() {
        let mut def = definition();
        def.cost_hint = CostHint::Destructive;
        let layer = ApprovalLayer::new(ApprovalPolicy::Always);

        assert!(layer.gate(&def, &invocation(Caller::Direct)).is_some());
        layer.approve_for_session(def.name.clone());
        assert!(layer.gate(&def, &invocation(Caller::Direct)).is_none());
    }

    #[test]
    fn secrets_are_stripped_from_arguments_before_they_are_logged() {
        let layer = RedactLayer::default();
        let redacted = layer.redact(&serde_json::json!({
            "url": "https://api.test",
            "headers": {"Authorization": "Bearer sk-real-secret"},
            "items": [{"api_key": "sk-another"}]
        }));
        let rendered = redacted.to_string();
        assert!(!rendered.contains("sk-real-secret"), "{rendered}");
        assert!(!rendered.contains("sk-another"), "nested and in arrays too: {rendered}");
        assert!(rendered.contains("https://api.test"), "and nothing else is touched");
    }

    #[test]
    fn truncation_reports_how_much_it_hid() {
        let layer = TruncateLayer { max_bytes: 10 };
        let (text, elided) = layer.apply("0123456789abcdef".to_string());
        assert_eq!(text, "0123456789");
        assert_eq!(elided, 6, "the model must be able to ask for the rest");

        let (whole, none) = layer.apply("short".to_string());
        assert_eq!(whole, "short");
        assert_eq!(none, 0);
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // Cutting mid-sequence would produce invalid UTF-8 and panic on the slice.
        let layer = TruncateLayer { max_bytes: 3 };
        let (text, elided) = layer.apply("aé".to_string());
        assert_eq!(text, "aé");
        assert_eq!(elided, 0);

        let layer = TruncateLayer { max_bytes: 2 };
        let (text, elided) = layer.apply("aé".to_string());
        assert_eq!(text, "a");
        assert_eq!(elided, 2);
    }
}
