//! Where a turn's time goes, with no API key and no cost.
//!
//! Run with `cargo run -p frey --example turn_timing`.
//!
//! **A scripted model answers instantly**, so `provider` collapses to roughly nothing and what is
//! left is the framework. That is the opposite of a realistic turn, and exactly the right condition
//! for measuring Frey's own cost: against a real provider this number is buried under a second of
//! network and inference, which is why nobody had it until now.
//!
//! The number to read is `OVERHEAD` — total minus the provider wait minus the caller's tools. It is
//! computed by subtraction rather than by adding the phases up, so anything happening in the loop
//! that nobody thought to instrument shows up as `unaccounted` instead of vanishing.
//!
//! # This is a cold-start number, and that is most of it
//!
//! One turn, in a fresh process, with nothing warmed. It reads around **280 µs**, and roughly 95%
//! of that is first-run cost: lazy initialisation, allocator growth, first-touch page faults. The
//! `concurrency` example warms up first and then measures the same overhead at **~12 µs**.
//!
//! Both are real and they answer different questions. This one is what your *first* turn costs;
//! that one is what every turn after it costs. Reporting this figure as "Frey's per-turn overhead"
//! would overstate it by more than twentyfold, which it did for about an hour on 2026-08-14.
//!
//! For a real run, `frey timings <journal.jsonl>` reports the same breakdown across every turn a
//! recorded run took.

use frey::prelude::*;
use frey_core::event::EventKind;
use frey_testkit::scripted::{ScriptedModel, Turn as Scripted};

/// No tools, so the `tools` row is honestly zero rather than measuring an example's own fake work.
struct NoTools;

impl ToolHost for NoTools {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>, ToolError> {
        Ok(Vec::new())
    }

    async fn call(
        &self,
        _invocation: Invocation,
        _cx: &ToolCx,
    ) -> ToolOutcome<frey_core::tool::ToolValue> {
        ToolOutcome::Failed(ToolError::new(ToolErrorKind::NotFound, "this agent has no tools"))
    }
}

fn main() {
    let agent =
        Agent::new(ScriptedModel::new(vec![Scripted::text("measured")]), NoTools, "scripted-model");
    let out = match pollster::block_on(agent.run("how long do you take?")) {
        Ok(out) => out,
        Err(error) => {
            eprintln!("run failed: {error}");
            return;
        }
    };

    for event in &out.journal.events {
        let EventKind::TurnFinished { turn, timing } = &event.kind else { continue };
        println!("turn {}\n", turn.0);
        for (label, value, whose) in [
            ("segment", timing.segment_us, "frey"),
            ("budget", timing.budget_us, "frey"),
            ("plan", timing.plan_us, "frey"),
            ("assemble", timing.assemble_us, "frey"),
            ("account", timing.account_us, "frey"),
            ("provider", timing.provider_us, "NOT frey"),
            ("tools", timing.tools_us, "NOT frey"),
        ] {
            println!("  {label:<12} {value:>8} µs   {whose}");
        }
        println!(
            "  {:<12} {:>8} µs   nobody put a clock here",
            "unaccounted",
            timing.overhead_us().saturating_sub(timing.accounted_us())
        );
        println!("  {:-<40}", "");
        println!("  {:<12} {:>8} µs   frey's share", "OVERHEAD", timing.overhead_us());
        println!("  {:<12} {:>8} µs", "turn total", timing.total_us);
        println!("\n  {}‰ of this turn was Frey.", timing.overhead_permille());
    }
}
