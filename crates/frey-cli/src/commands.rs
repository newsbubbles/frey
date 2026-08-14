//! What each command does.
//!
//! Every command has a `--json` form, because the primary consumer of a diagnostic tool in 2026 is
//! another program.

use std::process::ExitCode;

use frey::agent::journal::Journal;
use frey::core::event::{EventKind, TurnTiming};
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

/// `frey timings <journal>` — where a recorded run's time actually went.
///
/// **The counterpart to logging the breakdown at all.** A `TurnTiming` on every turn is only useful
/// if something reads it back; a number written to a file nobody can open is the same as a number
/// nobody wrote. deadnet's nightly journals are the intended input.
///
/// Reports the median rather than the mean, on purpose. One 30-second turn where a provider queued
/// drags a mean far enough to hide what the other ninety turns did, and the question here is what
/// the framework normally costs, not what the worst network hiccup cost.
pub fn timings(path: Option<&str>, json: bool) -> ExitCode {
    let Some(path) = path else {
        eprintln!("usage: frey timings <journal.jsonl|journal.json> [--json]");
        return ExitCode::FAILURE;
    };
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("could not read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Both shapes: one journal per file, or one per line. deadnet writes the first and a stream
    // writes the second, and guessing wrong should not be a parse error a person has to decode.
    let mut timings: Vec<TurnTiming> = Vec::new();
    let mut journals = 0usize;
    for candidate in std::iter::once(body.as_str()).chain(body.lines()) {
        let Ok(journal) = serde_json::from_str::<Journal>(candidate) else { continue };
        journals += 1;
        timings.extend(journal.events.iter().filter_map(|e| match &e.kind {
            EventKind::TurnFinished { timing, .. } => Some(*timing),
            _ => None,
        }));
        if journals == 1 && candidate.len() == body.len() {
            break; // the whole file parsed as one journal; the per-line pass would double-count
        }
    }

    if timings.is_empty() {
        eprintln!(
            "no turn timings in {path}. Journals written before TurnTiming existed have none, \
             which is the honest answer rather than a zero."
        );
        return ExitCode::FAILURE;
    }

    let med = |mut v: Vec<u64>| -> u64 {
        v.sort_unstable();
        v[v.len() / 2]
    };
    let total = med(timings.iter().map(|t| t.total_us).collect());
    let overhead = med(timings.iter().map(TurnTiming::overhead_us).collect());
    let unaccounted =
        med(timings.iter().map(|t| t.overhead_us().saturating_sub(t.accounted_us())).collect());

    if json {
        println!(
            "{}",
            serde_json::json!({
                "turns": timings.len(),
                "medianTotalUs": total,
                "medianOverheadUs": overhead,
                "medianUnaccountedUs": unaccounted,
                "medianSegmentUs": med(timings.iter().map(|t| t.segment_us).collect()),
                "medianBudgetUs": med(timings.iter().map(|t| t.budget_us).collect()),
                "medianPlanUs": med(timings.iter().map(|t| t.plan_us).collect()),
                "medianAssembleUs": med(timings.iter().map(|t| t.assemble_us).collect()),
                "medianAccountUs": med(timings.iter().map(|t| t.account_us).collect()),
                "medianProviderUs": med(timings.iter().map(|t| t.provider_us).collect()),
                "medianToolsUs": med(timings.iter().map(|t| t.tools_us).collect()),
            })
        );
        return ExitCode::SUCCESS;
    }

    println!("{} turn(s) across {journals} journal(s), medians in µs\n", timings.len());
    for (label, value) in [
        ("segment", med(timings.iter().map(|t| t.segment_us).collect())),
        ("budget", med(timings.iter().map(|t| t.budget_us).collect())),
        ("plan", med(timings.iter().map(|t| t.plan_us).collect())),
        ("assemble", med(timings.iter().map(|t| t.assemble_us).collect())),
        ("account", med(timings.iter().map(|t| t.account_us).collect())),
    ] {
        println!("  {label:<12} {value:>10}");
    }
    println!("  {:<12} {:>10}   <- nobody put a clock here", "unaccounted", unaccounted);
    println!("  {:-<24}", "");
    println!("  {:<12} {:>10}   frey", "overhead", overhead);
    println!(
        "  {:<12} {:>10}   not frey",
        "provider",
        med(timings.iter().map(|t| t.provider_us).collect())
    );
    println!(
        "  {:<12} {:>10}   not frey",
        "tools",
        med(timings.iter().map(|t| t.tools_us).collect())
    );
    println!("  {:<12} {total:>10}", "turn");
    // Parts per million, matching `TurnTiming::overhead_ppm`. This printed per-mille until the
    // rename and would have read `0` on any journal from a real run — the same defect as the type,
    // in the tool built to read the type, missed because the rename was done by method name.
    if let Some(ppm) = overhead.saturating_mul(1_000_000).checked_div(total) {
        println!("\nFrey is {ppm} ppm of a median turn here.");
    }
    ExitCode::SUCCESS
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

    /// The reader must survive a journal from before `TurnTiming` existed.
    ///
    /// Every deadnet journal on disk today is one. Reporting zeros for those would be inventing a
    /// measurement; it says it has none and exits non-zero.
    #[test]
    fn a_journal_with_no_timings_says_so_rather_than_reporting_zero() {
        let dir = std::env::temp_dir().join("frey-timings-test");
        std::fs::create_dir_all(&dir).expect("tempdir");
        let file = dir.join("old.json");
        std::fs::write(
            &file,
            serde_json::json!({"run": "r1", "entries": [], "events": []}).to_string(),
        )
        .expect("write");
        assert_eq!(
            timings(file.to_str(), true),
            ExitCode::FAILURE,
            "no data must not read as a fast run"
        );
    }

    #[test]
    fn a_missing_file_is_an_error_and_not_a_panic() {
        assert_eq!(timings(Some("nowhere-at-all.jsonl"), false), ExitCode::FAILURE);
        assert_eq!(timings(None, false), ExitCode::FAILURE);
    }

    #[test]
    fn every_profile_appears_in_the_listing() {
        assert!(profiles::all().len() >= 6, "the table should cover every shape of provider");
        assert!(profiles::all().iter().any(|(_, c)| c.reports_cost));
        assert!(profiles::all().iter().any(|(_, c)| !c.reports_cost));
    }
}
