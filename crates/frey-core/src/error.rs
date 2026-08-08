//! Failures, typed by audience.
//!
//! A tool failure has up to three readers, and collapsing them into one string is how frameworks
//! end up leaking stack traces into prompts and showing users text meant for a model:
//!
//! * the **model**, which needs to know what to do next ([`ModelMessage`]);
//! * the **operator**, who needs a diagnosis ([`Diagnostic`]);
//! * a **human user**, who may need a sentence in a UI ([`Presentation`]).
//!
//! Only [`ModelMessage`] ever enters the context window. That separation is enforced by a test.
//!
//! ```
//! use frey_core::prelude::*;
//! use frey_core::tool_err;
//!
//! let err = tool_err!(NotFound, "no file at src/main.rs")
//!     .guide("List the directory with `fs_list` before reading.")
//!     .suggest(["fs_list"]);
//!
//! assert!(!err.kind().is_retryable());
//! assert!(err.model().guidance.as_deref().unwrap().contains("fs_list"));
//! ```

use std::fmt;

use smol_str::SmolStr;

/// What the model is told. This is the only part of a failure that enters the context window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelMessage {
    /// One sentence describing what went wrong, in the model's terms.
    pub summary: String,
    /// What to do about it. This is the "custom message carrying further instruction" that makes
    /// a tool error recoverable rather than merely reported.
    pub guidance: Option<String>,
    /// Tools that would plausibly help. Feeds discovery as well as the prompt.
    pub suggested_tools: Vec<SmolStr>,
    /// A corrected or example argument shape, when the failure was a schema violation.
    pub schema_hint: Option<serde_json::Value>,
}

impl ModelMessage {
    /// A bare message with no guidance.
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            guidance: None,
            suggested_tools: Vec::new(),
            schema_hint: None,
        }
    }
}

impl fmt::Display for ModelMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary)?;
        if let Some(g) = &self.guidance {
            write!(f, " {g}")?;
        }
        Ok(())
    }
}

/// What the operator is told. Never enters the context window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostic {
    /// Free-form detail: a stack trace, an upstream body, a hostname.
    pub detail: String,
    /// Structured key/value context for logs and traces.
    pub fields: Vec<(SmolStr, SmolStr)>,
}

impl Diagnostic {
    /// A diagnostic with some detail.
    pub fn new(detail: impl Into<String>) -> Self {
        Self { detail: detail.into(), fields: Vec::new() }
    }

    /// Attach a structured field.
    #[must_use]
    pub fn field(mut self, key: impl Into<SmolStr>, value: impl Into<SmolStr>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }
}

/// What a human user is shown, when there is a UI and the failure is worth surfacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    /// A short, non-technical sentence.
    pub message: String,
}

/// The shape of a failure. Determines default retry behaviour and how loudly it is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolErrorKind {
    /// The arguments did not match the schema.
    InvalidArgs,
    /// The thing being addressed does not exist.
    NotFound,
    /// Policy refused. The model is told; the operator is alerted.
    Denied,
    /// The operation conflicted with concurrent state.
    Conflict,
    /// Took too long.
    Timeout,
    /// Might succeed if tried again.
    Transient,
    /// Will not succeed if tried again. Auth and billing failures live here.
    Fatal,
    /// The run was cancelled.
    Cancelled,
}

impl ToolErrorKind {
    /// Whether retrying could plausibly help. Auth and billing failures return `false`, which is
    /// what stops a credit-exhaustion 402 from being swallowed by a retry loop.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::Timeout | Self::Conflict)
    }

    /// Whether the operator should be alerted even though the model was told.
    #[must_use]
    pub fn alerts_operator(self) -> bool {
        matches!(self, Self::Denied | Self::Fatal)
    }
}

/// How the runtime should treat a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryDirective {
    /// Do not retry.
    Never,
    /// Retry immediately; `budgeted` decides whether it consumes the run's retry budget.
    Immediate {
        /// Whether this retry counts against the budget.
        budgeted: bool,
    },
    /// Retry after a delay, in milliseconds.
    After {
        /// Delay before the next attempt.
        millis: u64,
    },
    /// Cannot proceed without something from outside the run.
    RequiresInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Inner {
    kind: ToolErrorKind,
    model: ModelMessage,
    operator: Diagnostic,
    user: Option<Presentation>,
    retry: RetryDirective,
}

/// A tool failed.
///
/// The payload is boxed: failures are the cold path, and `Result<T, ToolError>` is returned by
/// every tool in the framework, so the success path should not carry the weight of four strings
/// and two vectors it will never use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(Box<Inner>);

impl ToolError {
    /// A failure of `kind`, with `summary` for the model.
    pub fn new(kind: ToolErrorKind, summary: impl Into<String>) -> Self {
        let retry = if kind.is_retryable() {
            RetryDirective::Immediate { budgeted: true }
        } else {
            RetryDirective::Never
        };
        Self(Box::new(Inner {
            kind,
            model: ModelMessage::new(summary),
            operator: Diagnostic::default(),
            user: None,
            retry,
        }))
    }

    /// The shape of the failure.
    #[must_use]
    pub fn kind(&self) -> ToolErrorKind {
        self.0.kind
    }

    /// What the model is told. The only part that enters the context window.
    #[must_use]
    pub fn model(&self) -> &ModelMessage {
        &self.0.model
    }

    /// What the operator is told.
    #[must_use]
    pub fn operator(&self) -> &Diagnostic {
        &self.0.operator
    }

    /// What a human user is shown, if anything.
    #[must_use]
    pub fn user(&self) -> Option<&Presentation> {
        self.0.user.as_ref()
    }

    /// How the runtime should retry.
    #[must_use]
    pub fn retry_directive(&self) -> RetryDirective {
        self.0.retry
    }

    /// Tell the model what to do next.
    #[must_use]
    pub fn guide(mut self, guidance: impl Into<String>) -> Self {
        self.0.model.guidance = Some(guidance.into());
        self
    }

    /// Point the model at tools that would help.
    #[must_use]
    pub fn suggest<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<SmolStr>,
    {
        self.0.model.suggested_tools.extend(tools.into_iter().map(Into::into));
        self
    }

    /// Show the model the argument shape it should have used.
    #[must_use]
    pub fn schema_hint(mut self, hint: serde_json::Value) -> Self {
        self.0.model.schema_hint = Some(hint);
        self
    }

    /// Attach operator-only detail.
    #[must_use]
    pub fn diagnose(mut self, diagnostic: Diagnostic) -> Self {
        self.0.operator = diagnostic;
        self
    }

    /// Attach a sentence for a human user.
    #[must_use]
    pub fn present(mut self, message: impl Into<String>) -> Self {
        self.0.user = Some(Presentation { message: message.into() });
        self
    }

    /// Override the retry directive.
    #[must_use]
    pub fn retry(mut self, retry: RetryDirective) -> Self {
        self.0.retry = retry;
        self
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.0.kind, self.0.model.summary)
    }
}

impl std::error::Error for ToolError {}

/// Build a [`ToolError`] concisely: `tool_err!(NotFound, "no file at {path}")`.
#[macro_export]
macro_rules! tool_err {
    ($kind:ident, $($arg:tt)*) => {
        $crate::error::ToolError::new(
            $crate::error::ToolErrorKind::$kind,
            format!($($arg)*),
        )
    };
}

/// The result of invoking a tool.
///
/// `Denied` is separate from `Failed` because a permission refusal must reach the model *and*
/// alert the operator, and because retry semantics differ: a denied call should never be retried
/// with the same arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolOutcome<T> {
    /// It worked.
    Ok(T),
    /// It failed; the model may try something else.
    Failed(ToolError),
    /// Policy refused.
    Denied(ToolError),
    /// The call cannot proceed without something from outside the run: a human approval, an MCP
    /// elicitation, an A2A auth challenge, or a frontend-executed tool.
    NeedsInput(NeedsInput),
}

impl<T> ToolOutcome<T> {
    /// Whether this outcome is a success.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// The model-facing message, if this outcome carries one.
    #[must_use]
    pub fn model_message(&self) -> Option<&ModelMessage> {
        match self {
            Self::Ok(_) | Self::NeedsInput(_) => None,
            Self::Failed(e) | Self::Denied(e) => Some(e.model()),
        }
    }
}

/// A run cannot continue without something from outside it.
///
/// One type, four projections: MCP's `input_required`, A2A's `INPUT_REQUIRED`/`AUTH_REQUIRED`,
/// AG-UI's interrupt, and a local approval prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsInput {
    /// Opaque, sealed token that lets the run resume exactly where it stopped.
    pub token: SmolStr,
    /// What is needed.
    pub requests: Vec<InputRequest>,
}

/// One thing the run needs before it can continue.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputRequest {
    /// A human must approve a specific action. `literal` is the exact command, URL, or statement —
    /// never a natural-language summary, because summaries are where injected actions hide.
    Approval {
        /// The exact action, verbatim.
        literal: String,
        /// How dangerous it is.
        risk: Risk,
    },
    /// A choice between options.
    Choice {
        /// What is being asked.
        prompt: String,
        /// The available answers.
        options: Vec<String>,
    },
    /// Structured input matching a schema (MCP elicitation).
    Form {
        /// JSON Schema describing the expected input.
        schema: serde_json::Value,
    },
    /// Authentication against a resource (A2A `AUTH_REQUIRED`).
    Auth {
        /// The resource requiring authentication.
        resource: String,
    },
    /// A tool the frontend must execute (AG-UI).
    FrontendTool {
        /// Tool name.
        name: SmolStr,
        /// Arguments.
        args: serde_json::Value,
    },
}

/// How dangerous an action is. Derived from a tool's declared capabilities and cost hint, never
/// from the model's own assessment of what it is about to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Read-only, reversible, cheap.
    Low,
    /// Writes, or costs money.
    Medium,
    /// Destructive, or leaves the machine.
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_success_path_does_not_pay_for_the_failure_path() {
        // A pointer plus the niche-optimised `Result` discriminant. If this grows, every tool in
        // the framework gets slower for no benefit.
        assert_eq!(size_of::<ToolError>(), size_of::<usize>());
        assert_eq!(size_of::<Result<(), ToolError>>(), size_of::<usize>());
    }

    #[test]
    fn auth_and_billing_failures_are_never_retryable() {
        let e = ToolError::new(ToolErrorKind::Fatal, "402 from provider");
        assert_eq!(e.retry_directive(), RetryDirective::Never);
        assert!(!e.kind().is_retryable());
        assert!(e.kind().alerts_operator());
    }

    #[test]
    fn transient_failures_default_to_a_budgeted_retry() {
        let e = ToolError::new(ToolErrorKind::Transient, "connection reset");
        assert_eq!(e.retry_directive(), RetryDirective::Immediate { budgeted: true });
    }

    #[test]
    fn denial_reaches_the_model_and_alerts_the_operator() {
        let e =
            ToolError::new(ToolErrorKind::Denied, "writing outside the workspace is not permitted")
                .guide("Write under ./out instead, or ask the operator to widen fs:write.");
        assert!(e.kind().alerts_operator());
        let outcome: ToolOutcome<()> = ToolOutcome::Denied(e);
        let msg = outcome.model_message().expect("a denial tells the model why");
        assert!(msg.guidance.as_deref().unwrap().contains("./out"));
    }

    #[test]
    fn operator_detail_never_appears_in_the_model_message() {
        let e = tool_err!(NotFound, "no file at src/main.rs")
            .guide("List the directory with `fs_list` first.")
            .suggest(["fs_list"])
            .diagnose(
                Diagnostic::new("ENOENT from openat(2) on /home/nate/secret-project/src/main.rs")
                    .field("errno", "2"),
            );

        let model_text = format!("{}", e.model());
        assert!(!model_text.contains("/home/nate"), "operator detail leaked: {model_text}");
        assert!(!model_text.contains("errno"), "operator detail leaked: {model_text}");
        assert!(model_text.contains("fs_list"));
        assert!(e.operator().detail.contains("/home/nate"));
    }

    #[test]
    fn schema_violations_can_show_the_model_the_right_shape() {
        let e = tool_err!(InvalidArgs, "expected an array of strings, got {}", "a string")
            .schema_hint(serde_json::json!({"type": "array", "items": {"type": "string"}}));
        assert_eq!(e.kind(), ToolErrorKind::InvalidArgs);
        assert!(e.model().schema_hint.is_some());
    }
}
