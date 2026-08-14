//! What each command does.
//!
//! Every command has a `--json` form, because the primary consumer of a diagnostic tool in 2026 is
//! another program.

use std::process::ExitCode;

use frey::harness::doctor::{Finding, Report, check_cost_reporting, check_sandbox};
use frey::profiles;
use frey::sandbox::probe::{
    LandlockAbi, linux_availability, macos_availability, windows_availability,
};

/// The help text.
#[must_use]
pub fn help() -> String {
    "frey — a Rust agent framework where the context window is a managed resource\n\
     \n\
     USAGE\n    \
       frey <command> [--json]\n\
     \n\
     COMMANDS\n    \
       doctor     Diagnose this host: confinement, cost reporting, what is missing\n    \
       profiles   Show what each supported model can do, and where it caches from\n    \
       tools      Report which tools in a catalog would be hard to find\n    \
       version    Print the version\n    \
       help       Print this\n\
     \n\
     FLAGS\n    \
       --json     Machine-readable output. `doctor --json` is a stable API.\n"
        .to_string()
}

fn emit(report: &Report, json: bool) -> ExitCode {
    if json {
        println!("{}", serde_json::to_string_pretty(&report.to_json()).unwrap_or_default());
    } else {
        for finding in &report.findings {
            let marker = match finding.severity {
                frey::harness::doctor::Severity::Error => "error",
                frey::harness::doctor::Severity::Warn => " warn",
                frey::harness::doctor::Severity::Info => " info",
            };
            println!("{marker}  {}  {}", finding.check, finding.message);
            if let Some(fix) = &finding.fix {
                println!("       fix: {fix}");
            }
        }
        if report.findings.is_empty() {
            println!(" info  nothing to report");
        }
    }

    if report.is_healthy() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Diagnose the host.
///
/// The confinement check is the one that matters: it reports what this machine can *actually*
/// enforce, which on Linux depends on a kernel ABI level and on whether the LSM was enabled at boot.
#[must_use]
pub fn doctor(json: bool) -> ExitCode {
    let mut report = Report::default();

    let availability = if cfg!(target_os = "linux") {
        // Detecting the real ABI level needs a syscall this crate does not make; until it does, the
        // honest answer is that the level is unknown rather than a number that might be wrong.
        linux_availability(LandlockAbi::NONE, false)
    } else if cfg!(target_os = "macos") {
        macos_availability()
    } else {
        windows_availability(false)
    };
    report.findings.extend(check_sandbox(availability.usable, &availability.detail));
    report.findings.push(Finding::info(
        "security.enforced",
        format!("controls actually available here: {:?}", availability.controls.controls()),
    ));

    // Measured, not tabulated: each dialect encodes a representative request and the markers in
    // the result are counted. It is the check that found OpenAI declaring an explicit breakpoint
    // mode the Responses API does not have.
    let survey = frey::providers::marks::survey();
    let rows: Vec<(&str, u8, usize, bool)> =
        survey.iter().map(|s| (s.provider, s.budget, s.realised, s.automatic)).collect();
    report.findings.extend(frey::harness::doctor::check_cache_marks(&rows));

    report.findings.extend(check_cost_reporting(&profiles::opus5()));
    report.findings.push(Finding::info(
        "profiles.checked",
        format!(
            "provider figures were last verified against vendor documentation on {}",
            profiles::CHECKED
        ),
    ));

    report.sort();
    emit(&report, json)
}

/// Show what each supported model can do.
#[must_use]
pub fn profiles(json: bool) -> ExitCode {
    if json {
        let rows: Vec<serde_json::Value> = profiles::all()
            .into_iter()
            .map(|(name, caps)| {
                serde_json::json!({
                    "name": name,
                    "minCacheablePrefix": caps.cache.min_prefix_tokens(),
                    "cacheBreakpoints": caps.cache.breakpoint_budget(),
                    "nativeToolSearch": caps.tool_search.is_native(),
                    "programmaticToolCalling": caps.programmatic_tool_calling,
                    "reportsCost": caps.reports_cost,
                    "maxContext": caps.max_context,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows).unwrap_or_default());
    } else {
        println!(
            "{:<22} {:>10} {:>12} {:>8} {:>7}",
            "model", "min-prefix", "breakpoints", "search", "cost"
        );
        for (name, caps) in profiles::all() {
            println!(
                "{name:<22} {:>10} {:>12} {:>8} {:>7}",
                caps.cache.min_prefix_tokens().map_or_else(|| "-".into(), |m| m.to_string()),
                caps.cache.breakpoint_budget(),
                if caps.tool_search.is_native() { "native" } else { "local" },
                if caps.reports_cost { "yes" } else { "est." },
            );
        }
        println!(
            "\nFigures verified against vendor documentation on {}. `cost: est.` means the provider\n\
             reports tokens but not money, so any figure Frey shows is an estimate.",
            profiles::CHECKED
        );
    }
    ExitCode::SUCCESS
}

/// Report which tools would be hard to find.
///
/// Reads a catalog as JSON on standard input, so it composes with whatever produced it.
#[must_use]
pub fn tools(json: bool) -> ExitCode {
    use std::io::Read as _;

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        eprintln!(
            "frey tools reads a tool catalog as JSON on stdin.\n\
             Example: frey tools --json < catalog.json"
        );
        return ExitCode::FAILURE;
    }

    let Ok(definitions) = serde_json::from_str::<Vec<frey::core::tool_def::ToolDefinition>>(&input)
    else {
        eprintln!("that is not a JSON array of tool definitions");
        return ExitCode::FAILURE;
    };

    let mut report = Report::default();
    report.findings.extend(frey::harness::doctor::check_discoverability(&definitions));
    if report.findings.is_empty() {
        report.findings.push(Finding::info(
            "tools.discoverability",
            format!("all {} tools are well described", definitions.len()),
        ));
    }
    report.sort();
    emit(&report, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_every_command_the_parser_accepts() {
        // A command that exists but is undocumented might as well not exist.
        let text = help();
        for command in ["doctor", "profiles", "tools", "version", "help"] {
            assert!(text.contains(command), "`{command}` is missing from help");
        }
        assert!(text.contains("--json"));
    }

    #[test]
    fn help_says_the_json_output_is_stable() {
        // Because a coding agent will parse it, and it needs to know it may.
        assert!(help().contains("stable API"));
    }

    #[test]
    fn an_unhealthy_report_exits_non_zero() {
        // So `frey doctor` composes with CI and shell chaining rather than needing its output
        // parsed to find out whether it passed.
        let mut broken = Report::default();
        broken.findings.extend(check_sandbox(false, "none available"));
        assert!(!broken.is_healthy());

        let mut fine = Report::default();
        fine.findings.push(Finding::info("x", "fine"));
        assert!(fine.is_healthy());
    }

    #[test]
    fn every_profile_appears_in_the_listing() {
        assert!(profiles::all().len() >= 6, "the table should cover every shape of provider");
        assert!(profiles::all().iter().any(|(_, c)| c.reports_cost));
        assert!(profiles::all().iter().any(|(_, c)| !c.reports_cost));
    }
}
