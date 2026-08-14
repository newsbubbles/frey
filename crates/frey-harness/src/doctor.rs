//! `frey doctor`: the fastest path from "something is wrong" to "here is what".
//!
//! This is the highest-value feature in the framework for R13 — a coding agent landing in an
//! unfamiliar Frey project can orient with one command instead of reading the source. That is why
//! the JSON output is treated as an API and snapshot-tested as one.
//!
//! Every check answers a question someone actually asks, and every failure carries a fix. A
//! diagnostic that says "misconfigured" has told you nothing you did not already know.

use frey_core::provider_caps::ProviderCapabilities;
use frey_core::tool_def::{Discoverability, ToolDefinition};
use serde_json::{Value, json};

/// How serious a finding is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing.
    Info,
    /// Working, but costing more than it should, or fragile.
    Warn,
    /// Broken.
    Error,
}

/// One thing `doctor` found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    /// Which check produced it.
    pub check: String,
    /// How serious.
    pub severity: Severity,
    /// What is true.
    pub message: String,
    /// What to do. Present on every non-`Info` finding, because a diagnostic without a fix is just
    /// a complaint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Finding {
    /// An informational finding.
    pub fn info(check: &str, message: impl Into<String>) -> Self {
        Self { check: check.into(), severity: Severity::Info, message: message.into(), fix: None }
    }

    /// A warning, with its fix.
    pub fn warn(check: &str, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            severity: Severity::Warn,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }

    /// An error, with its fix.
    pub fn error(check: &str, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            severity: Severity::Error,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Everything `doctor` found.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    /// The findings, most serious first.
    pub findings: Vec<Finding>,
}

impl Report {
    /// Whether anything is broken.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        !self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    /// The findings as an agent-facing JSON document.
    ///
    /// Stable by contract: a coding agent parses this, so a field rename is a breaking change and
    /// the snapshot test exists to make that visible.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "healthy": self.is_healthy(),
            "findings": self.findings,
        })
    }

    /// Sort most serious first, so a truncated read still shows what matters.
    pub fn sort(&mut self) {
        self.findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    }
}

/// Check that the catalog is findable.
///
/// A tool nobody can retrieve is a real defect once a catalog outgrows the context window, not a
/// style preference — which is why this is a check rather than a lint people turn off.
#[must_use]
pub fn check_discoverability(tools: &[ToolDefinition]) -> Vec<Finding> {
    tools
        .iter()
        .filter_map(|tool| {
            let report = tool.discoverability();
            if report.is_clean() {
                return None;
            }
            let problems: Vec<String> = report.problems.iter().map(describe).collect();
            Some(Finding::warn(
                "tools.discoverability",
                format!("`{}` would be hard to find: {}", tool.name, problems.join("; ")),
                "Tool search matches names, descriptions, argument names and argument \
                 descriptions. Document every parameter and give the tool a full sentence.",
            ))
        })
        .collect()
}

fn describe(problem: &Discoverability) -> String {
    match problem {
        Discoverability::NoDescription => "it has no description".into(),
        Discoverability::ThinDescription { words } => {
            format!("its description is only {words} words")
        }
        Discoverability::UndocumentedParameters { names } => {
            format!(
                "these parameters are undocumented: {}",
                names.iter().map(smol_str::SmolStr::as_str).collect::<Vec<_>>().join(", ")
            )
        }
        Discoverability::NoNamespace => {
            "it is deferred but has no `service_` prefix, so one search cannot match its group"
                .into()
        }
        _ => "it has an unrecognised discoverability problem".into(),
    }
}

/// Check that the prompt will actually be cached.
///
/// The failure this catches is invisible from the provider's side: a prefix below the model's
/// minimum is accepted and silently not cached, and the only symptom is the bill.
#[must_use]
pub fn check_cacheability(prefix_tokens: u32, caps: &ProviderCapabilities) -> Vec<Finding> {
    let Some(minimum) = caps.cache.min_prefix_tokens() else {
        return vec![Finding::info(
            "context.cache",
            "this model does not cache prompts, so every turn pays full price",
        )];
    };

    if prefix_tokens < minimum {
        return vec![Finding::warn(
            "context.cache",
            format!(
                "the stable prefix is {prefix_tokens} tokens; this model caches from {minimum}. \
                 The provider accepts it and silently does not cache, so the only symptom is cost."
            ),
            "Move more of the prompt into the stable prefix, or accept that caching is off for \
             this model.",
        )];
    }
    vec![Finding::info(
        "context.cache",
        format!("the stable prefix is {prefix_tokens} tokens, above this model's {minimum}"),
    )]
}

/// Report what each dialect does with a cache plan.
///
/// `support` comes from `frey_providers::marks::survey`, which encodes a real request and counts the
/// `cache_control` markers that come out — a measurement rather than a table. It lives here as a
/// `doctor` check because "does my cache plan do anything on this provider" is a question the
/// README's opening paragraph implicitly answered *yes* to for every provider, and the true answer
/// is one dialect of three.
///
/// Takes `(provider, budget, realised, automatic)` tuples rather than the type itself, so
/// `frey-harness` need not depend on `frey-providers` — the crate graph has harness above core and
/// beside providers, and inverting that for a diagnostic would be the wrong trade.
#[must_use]
pub fn check_cache_marks(support: &[(&str, u8, usize, bool)]) -> Vec<Finding> {
    support
        .iter()
        .map(|(provider, budget, realised, automatic)| {
            let check = "context.marks";
            match (*budget, *realised, *automatic) {
                // Declared a budget and emitted nothing. The whole reason this check exists.
                (b, 0, _) if b > 0 => Finding::error(
                    check,
                    format!(
                        "{provider} accepts {b} cache breakpoint(s) and realises none of them; \
                         every plan for it is discarded between the planner and the wire"
                    ),
                    "This is a bug in the adapter, not in your prompt. Until it is fixed, treat \
                     this provider as uncached.",
                ),
                (0, _, true) => Finding::info(
                    check,
                    format!(
                        "{provider} places no breakpoints and caches the prefix itself; the \
                         planner's churn and minimum-prefix warnings apply, its breakpoints do not"
                    ),
                ),
                (0, _, false) => Finding::warn(
                    check,
                    format!(
                        "{provider} takes no breakpoints and does not cache: every turn pays full \
                         price"
                    ),
                    "Use a caching provider, or accept the cost.",
                ),
                (b, n, _) => Finding::info(
                    check,
                    format!(
                        "{provider} allows {b} breakpoint(s) and realises {n} on a real request"
                    ),
                ),
            }
        })
        .collect()
}

/// An adapter that drops the breakpoints it accepts is an error, not a note.
///
/// The distinction is the point of the whole check: *no breakpoints* is a fact about a provider and
/// *breakpoints discarded* is a bug in Frey, and until this existed they produced the same
/// observable behaviour — an empty plan, a full-price bill, and no diagnostic anywhere.
#[cfg(test)]
mod mark_tests {
    use super::*;

    #[test]
    fn a_dropped_breakpoint_is_an_error_and_an_absent_one_is_not() {
        let findings = check_cache_marks(&[
            ("broken", 4, 0, false),
            ("automatic", 0, 0, true),
            ("working", 4, 4, false),
        ]);
        assert_eq!(findings[0].severity, Severity::Error, "{:?}", findings[0]);
        assert_eq!(findings[1].severity, Severity::Info);
        assert_eq!(findings[2].severity, Severity::Info);
        assert!(findings[0].message.contains("discarded"), "{}", findings[0].message);
    }
}

/// Check what confinement is actually available.
#[must_use]
pub fn check_sandbox(available: bool, detail: &str) -> Vec<Finding> {
    if available {
        vec![Finding::info("security.sandbox", detail.to_string())]
    } else {
        vec![Finding::error(
            "security.sandbox",
            format!("no usable sandbox backend: {detail}"),
            "Tools that execute anything will refuse to run. Install a backend, or set \
             security.allow_degraded_sandbox deliberately to accept reduced confinement.",
        )]
    }
}

/// Check whether costs will be complete.
#[must_use]
pub fn check_cost_reporting(caps: &ProviderCapabilities) -> Vec<Finding> {
    if caps.reports_cost {
        vec![Finding::info("cost.reporting", "this provider reports what each call cost")]
    } else {
        vec![Finding::warn(
            "cost.reporting",
            "this provider reports tokens but not money, so any figure Frey shows is an estimate",
            "Read cost figures as estimates, or use a provider that reports cost if exact \
             accounting matters.",
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_context::profiles;
    use frey_core::tool_def::JsonSchema;

    fn good_tool() -> ToolDefinition {
        ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents as text",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Path from the root."}}
            }))
            .unwrap(),
        )
    }

    #[test]
    fn a_healthy_project_reports_nothing_to_fix() {
        let findings = check_discoverability(&[good_tool()]);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn an_undocumented_parameter_is_reported_with_the_reason_it_matters() {
        let vague = ToolDefinition::new(
            "doit",
            "Does it",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"x": {"type": "string"}}
            }))
            .unwrap(),
        );
        let findings = check_discoverability(&[vague]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("undocumented"), "{}", findings[0].message);
        assert!(findings[0].fix.as_ref().unwrap().contains("argument descriptions"));
    }

    #[test]
    fn a_prefix_below_the_minimum_is_caught_because_the_provider_will_not_say() {
        // Accepted, silently uncached, and the only symptom is the bill.
        let findings = check_cacheability(380, &profiles::haiku45());
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("4096"), "{}", findings[0].message);
        assert!(findings[0].message.contains("silently"), "say why it is invisible");
    }

    #[test]
    fn a_sufficient_prefix_is_confirmed_rather_than_left_ambiguous() {
        let findings = check_cacheability(20_000, &profiles::opus5());
        assert_eq!(findings[0].severity, Severity::Info);
    }

    #[test]
    fn a_model_that_cannot_cache_says_so_once() {
        let findings = check_cacheability(20_000, &profiles::no_cache());
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].message.contains("full price"));
    }

    #[test]
    fn no_sandbox_is_an_error_rather_than_a_warning() {
        // Because tools that execute anything will refuse to run: that is a broken project, not a
        // suboptimal one.
        let findings = check_sandbox(false, "landlock unavailable, kernel 5.14");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].fix.as_ref().unwrap().contains("allow_degraded_sandbox"));
    }

    #[test]
    fn estimated_costs_are_flagged_as_estimates() {
        assert_eq!(check_cost_reporting(&profiles::opus5())[0].severity, Severity::Warn);
        assert_eq!(
            check_cost_reporting(&profiles::openrouter_automatic())[0].severity,
            Severity::Info
        );
    }

    #[test]
    fn every_actionable_finding_carries_a_fix() {
        // A diagnostic without a fix is a complaint.
        let mut report = Report::default();
        report.findings.extend(check_sandbox(false, "none"));
        report.findings.extend(check_cacheability(10, &profiles::haiku45()));
        report.findings.extend(check_cost_reporting(&profiles::opus5()));

        for finding in &report.findings {
            if finding.severity != Severity::Info {
                assert!(finding.fix.is_some(), "{} has no fix", finding.check);
            }
        }
    }

    #[test]
    fn the_report_sorts_the_worst_first() {
        let mut report = Report::default();
        report.findings.extend(check_cost_reporting(&profiles::opus5()));
        report.findings.extend(check_sandbox(false, "none"));
        report.sort();
        assert_eq!(report.findings[0].severity, Severity::Error);
        assert!(!report.is_healthy());
    }

    #[test]
    fn the_json_shape_is_an_api_and_is_pinned() {
        // A coding agent parses this, so a field rename is a breaking change.
        let mut report = Report::default();
        report.findings.push(Finding::warn("a.b", "something", "do this"));

        let json = report.to_json();
        assert_eq!(json["healthy"], serde_json::json!(true));
        assert_eq!(json["findings"][0]["check"], serde_json::json!("a.b"));
        assert_eq!(json["findings"][0]["severity"], serde_json::json!("warn"));
        assert_eq!(json["findings"][0]["fix"], serde_json::json!("do this"));

        // Info findings omit `fix` rather than sending null, so a consumer can branch on presence.
        let mut plain = Report::default();
        plain.findings.push(Finding::info("a.b", "fine"));
        assert!(plain.to_json()["findings"][0].get("fix").is_none());
    }
}
