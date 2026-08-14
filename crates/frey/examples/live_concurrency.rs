//! Concurrency against a **real** provider, with real money.
//!
//! ```text
//! OPENROUTER_API_KEY=... cargo run --release -p frey --example live_concurrency
//! ```
//!
//! The `concurrency` example answers "does Frey's own per-turn cost degrade under load" against a
//! provider that sleeps. That is the right way to isolate the framework and it deliberately says
//! nothing about sockets, TLS, HTTP/2 stream limits, rate limits or DNS — and a real fleet meets all
//! five. This is the other half.
//!
//! Two numbers come out of it that the fake cannot produce:
//!
//! 1. **The real ratio.** With a sleeping provider `provider_us` is a constant you chose. Here it is
//!    a live model, so `overhead_ppm` finally means something: what fraction of an actual turn
//!    is the framework.
//! 2. **Whether concurrency breaks anything a fake cannot break.** Rate limits, connection-pool
//!    exhaustion, and a router that answers 402 halfway through. Failures are counted and reported
//!    rather than averaged out of existence, because a run that quietly stops working still returns.
//!
//! # Cost
//!
//! Flash-tier models only, one turn each, no tools, output capped hard. `MAX_REQUESTS` bounds the
//! whole run: **Frey has no spend cap of its own** — there is no `max_cost` anywhere in thirteen
//! crates — so the ceiling has to live in the caller, and this is the caller. At the levels below
//! the whole sweep is a fraction of a cent per model.
//!
//! Reasoning is switched **off** explicitly. A thinking model that spends its budget on hidden
//! reasoning bills the full output cap and returns empty content with `finish_reason: length`,
//! which would be paid for and measure nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use frey::prelude::*;
use frey_core::event::EventKind;

/// Models to sweep. Flash tier, pinned rather than floating: a measurement against
/// `~vendor/model-latest` is a measurement of whatever that pointed at on the day.
const MODELS: &[&str] = &["qwen/qwen3.7-flash", "deepseek/deepseek-v4-flash"];

/// Concurrency levels. Modest on purpose — every one of these is a paid API call, and the
/// interesting question is the shape, not the peak.
const LEVELS: &[usize] = &[1, 4, 16, 32];

/// **The spend cap, since the framework has none.** The sweep refuses to start if the levels would
/// exceed it, so editing `LEVELS` upward cannot quietly turn a penny into a bill.
const MAX_REQUESTS: usize = 128;

/// A newline, named rather than inline.
const NL: char = '\n';

/// Small enough that a model cannot run away with the output budget.
const MAX_OUTPUT_HINT: &str = "Answer with a single word.";

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
        ToolOutcome::Failed(ToolError::new(ToolErrorKind::NotFound, "no tools in this measurement"))
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("OPENROUTER_API_KEY is not set — this example spends real money and needs one.");
        return;
    }

    let per_model: usize = LEVELS.iter().sum();
    let planned = per_model * MODELS.len();
    if planned > MAX_REQUESTS {
        eprintln!("refusing to start: {planned} requests planned, cap is {MAX_REQUESTS}");
        return;
    }
    println!(
        "{planned} paid requests planned across {} model(s), cap {MAX_REQUESTS}\n",
        MODELS.len()
    );

    // A dated record, so a claim resting on this can expire. Same reasoning as the conformance
    // sweep: a claim whose evidence cannot go stale is one that will eventually be wrong quietly —
    // and a live measurement ages faster than a free one, because the provider changes its fleet
    // underneath you without telling anybody.
    let day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400);
    let mut rows: Vec<String> = Vec::new();

    for model in MODELS {
        println!("== {model} ==\n");
        println!(
            "{:>7}  {:>11}  {:>11}  {:>9}  {:>7}  {:>6}",
            "agents", "overhead µs", "provider ms", "frey ppm", "wall ms", "failed"
        );
        println!("{:->7}  {:->11}  {:->11}  {:->9}  {:->7}  {:->6}", "", "", "", "", "", "");

        let mut spent_micros = 0i64;
        let mut billed_turns = 0u64;
        let mut total_failures = 0u64;
        // The busiest level is the one worth recording: one agent measures a cold start.
        let (mut best_overhead, mut best_provider, mut best_ppm) = (0u64, 0u64, 0u64);

        for &n in LEVELS {
            // **One adapter, shared.** The affordance under test, and the reason `with_client`
            // exists: one `reqwest::Client` per agent multiplies connection pools until a fleet
            // fails as socket exhaustion, which does not look like a client problem.
            let provider = Arc::new(
                HttpProvider::new(
                    Arc::new(OpenRouter::new()),
                    "https://openrouter.ai/api/v1",
                    Auth::Bearer { env: "OPENROUTER_API_KEY".into() },
                )
                .expect("the HTTP client builds"),
            );
            let failures = Arc::new(AtomicU64::new(0));

            let started = std::time::Instant::now();
            let mut tasks = Vec::with_capacity(n);
            for i in 0..n {
                let shared = Arc::clone(&provider);
                let failures = Arc::clone(&failures);
                let model = (*model).to_string();
                tasks.push(tokio::spawn(async move {
                    let agent = Agent::new(shared, NoTools, model)
                        .max_turns(1)
                        // Distinct sessions: many short sessions sharing one prefix is the shape
                        // this framework is aimed at, and it is also what makes the run ids
                        // distinguishable in the journals.
                        .session(frey_core::ids::SessionId::new(format!("probe-{i}")))
                        .system(MAX_OUTPUT_HINT)
                        .extra("reasoning", serde_json::json!({"enabled": false}));
                    match agent.run("Name a colour.").await {
                        Ok(out) => Some(out),
                        Err(error) => {
                            // Counted, never swallowed. A 402 halfway through a sweep degrades
                            // every remaining row and looks exactly like a fast one.
                            eprintln!("  run failed: {error}");
                            failures.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                    }
                }));
            }

            let mut overheads = Vec::new();
            let mut providers = Vec::new();
            let mut ppm = Vec::new();
            for task in tasks {
                let Ok(Some(out)) = task.await else {
                    failures.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                if let Some(cost) = out.cost {
                    // Micros, integer, because this figure ends up in a document. The provider
                    // reports it; Frey never invents one, so an absent cost stays absent.
                    spent_micros = spent_micros.saturating_add(cost.amount.micros);
                }
                for event in &out.journal.events {
                    let EventKind::TurnFinished { timing, .. } = &event.kind else { continue };
                    overheads.push(timing.overhead_us());
                    providers.push(timing.provider_us);
                    ppm.push(timing.overhead_ppm());
                    billed_turns += 1;
                }
            }
            let wall = started.elapsed();
            overheads.sort_unstable();
            providers.sort_unstable();
            ppm.sort_unstable();

            total_failures = total_failures.saturating_add(failures.load(Ordering::Relaxed));
            if n == *LEVELS.last().unwrap_or(&0) {
                best_overhead = percentile(&overheads, 50);
                best_provider = percentile(&providers, 50) / 1000;
                best_ppm = percentile(&ppm, 50);
            }
            println!(
                "{n:>7}  {:>11}  {:>11}  {:>9}  {:>7}  {:>6}",
                percentile(&overheads, 50),
                percentile(&providers, 50) / 1000,
                percentile(&ppm, 50),
                wall.as_millis(),
                failures.load(Ordering::Relaxed),
            );
        }

        // Built by serde rather than by hand: the hand-written version leaked the source file's
        // own indentation into the record, which is legal JSON and unreadable evidence.
        rows.push(
            serde_json::json!({
                "day": day,
                "model": model,
                "levels": LEVELS,
                "concurrency": LEVELS.last().copied().unwrap_or(0),
                "turns": billed_turns,
                "failures": total_failures,
                "overhead_us_median": best_overhead,
                "provider_ms_median": best_provider,
                "frey_ppm": best_ppm,
                "cost_micros": spent_micros,
            })
            .to_string(),
        );

        #[expect(
            clippy::cast_precision_loss,
            reason = "micros to dollars for one printed line; the integer is the record"
        )]
        let dollars = spent_micros as f64 / 1e6;
        println!("\n  {billed_turns} turn(s) billed, ${dollars:.5} reported by the provider.\n");
    }

    let dir = std::path::Path::new("notes/perf");
    if std::fs::create_dir_all(dir).is_ok() {
        let path = dir.join("live-concurrency.jsonl");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let today = format!("\"day\": {day},");
        let mut out: String = existing
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.contains(&today))
            .fold(String::new(), |mut acc, l| {
                acc.push_str(l);
                acc.push(NL);
                acc
            });
        for row in &rows {
            out.push_str(row);
            out.push(NL);
        }
        if std::fs::write(&path, out).is_ok() {
            println!(
                "wrote {}
",
                path.display()
            );
        }
    }

    println!(
        "`frey ppm` is the framework's share of a real turn, in parts per million. The fake sweep\n\
         could not produce that number; this one can, and it is the only figure here worth putting\n\
         next to another framework."
    );
}
