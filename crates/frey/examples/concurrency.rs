//! What happens to Frey's own per-turn cost when many agents run at once.
//!
//! Run with `cargo run --release -p frey --example concurrency`.
//!
//! The question comes from a real workload: an external reviewer building a simulation of roughly
//! **35,000 sessions a day**, many short runs sharing one prefix and one provider adapter. Two
//! affordances exist for exactly that shape — `complete` takes `&self`, and `Arc<P>` is itself a
//! `ModelProvider`, so one adapter behind an `Arc` serves any number of agents rather than each
//! agent building its own connection pool. Neither had ever been measured.
//!
//! # What this measures, and what it cannot
//!
//! Every agent shares **one** provider instance. The provider sleeps for a fixed time and then
//! answers, which stands in for network and inference: without it the runs finish instantly, never
//! overlap, and "concurrency" measures nothing. With it, all N runs are genuinely in flight at once.
//!
//! The number to watch is **overhead per turn**, from the same `TurnTiming` the loop already emits.
//! If Frey's own work is unaffected by how many agents are running, it stays flat as N grows. If it
//! does not, the phase breakdown says which part gave way — and that is the whole reason to reuse
//! the meters rather than time the wall clock and guess.
//!
//! It cannot tell you about a real provider: no sockets, no TLS, no HTTP/2 stream limits, no
//! rate limits, no DNS. A flat line here means *Frey* does not degrade, not that your provider will
//! not. Those are different claims and only the first one is being made.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use frey::prelude::*;
use frey_core::event::EventKind;
use frey_core::ids::{ModelId, ProviderId};
use frey_core::item::TextItem;
use frey_core::provider::{
    EventStream, ModelProvider, ProviderError, Request, Response, StopReason,
};
use frey_core::provider_caps::ProviderCapabilities;
use frey_core::taint::Provenance;
use frey_core::usage::Usage;

/// How long the fake provider pretends the network and the model take.
const LATENCY: Duration = Duration::from_millis(50);

/// Concurrency levels to sweep. Each level runs that many agents against one shared adapter.
const LEVELS: &[usize] = &[1, 8, 64, 256, 1024];

/// A provider with no state to contend over.
///
/// **Deliberately not `ScriptedModel`.** That one keeps a script cursor behind a `Mutex`, so a
/// thousand agents sharing it would queue on that lock and the measurement would be of the test
/// harness rather than of Frey. This answers the same thing every time from `&self` with no
/// interior mutability at all, except one atomic counter that exists only to prove every agent
/// really did reach the provider.
struct SharedModel {
    calls: AtomicU64,
}

impl ModelProvider for SharedModel {
    fn id(&self) -> ProviderId {
        ProviderId::new("shared")
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        // A realistic-ish window so the budgeter has something to reason about rather than
        // trivially fitting or trivially failing.
        ProviderCapabilities::minimal(200_000, 4_096)
    }

    async fn complete(&self, request: Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(LATENCY).await;
        Ok(Response {
            items: vec![Item::Text(TextItem {
                text: "done".into(),
                provenance: Some(Provenance::new("provider:shared")),
            })],
            usage: Usage { input: 100, output: 4, ..Usage::default() },
            stop: StopReason::EndTurn,
            model: request.model,
            provider: self.id(),
        })
    }

    /// The loop does not stream, so this exists to satisfy the trait and says so rather than
    /// pretending to work.
    async fn stream(&self, _request: Request) -> Result<EventStream, ProviderError> {
        Err(ProviderError::Unsupported { provider: self.id(), capability: "streaming".into() })
    }
}

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
        ToolOutcome::Failed(ToolError::new(ToolErrorKind::NotFound, "no tools here"))
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
    println!(
        "one shared adapter, {} ms of simulated provider latency, {} worker threads\n",
        LATENCY.as_millis(),
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    println!(
        "{:>6}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "agents", "median µs", "p99 µs", "segment", "assemble", "wall ms"
    );
    println!("{:->6}  {:->10}  {:->10}  {:->10}  {:->10}  {:->10}", "", "", "", "", "", "");

    // **Warm up first, and this is not a formality.** The first run in a fresh process pays for
    // lazy initialisation, allocator growth and first-touch page faults, and on the first version
    // of this sweep that made one agent look 20x more expensive per turn than a thousand — a result
    // that reads as "Frey gets faster under load" and is really "the first measurement in a process
    // is a cold one". `turn_timing` measures exactly one turn in a fresh process, so its number is
    // a cold-start figure too, and `docs/performance.md` now says so.
    for _ in 0..64 {
        let warm = Arc::new(SharedModel { calls: AtomicU64::new(0) });
        let _ = Agent::new(warm, NoTools, "shared-model").run("warm").await;
    }

    let mut baseline = 0u64;
    for &n in LEVELS {
        // **One provider. Not one per agent.** That is the affordance under test.
        let provider = Arc::new(SharedModel { calls: AtomicU64::new(0) });

        let started = Instant::now();
        let mut tasks = Vec::with_capacity(n);
        for _ in 0..n {
            let shared = Arc::clone(&provider);
            tasks.push(tokio::spawn(async move {
                Agent::new(shared, NoTools, "shared-model").run("go").await
            }));
        }

        let mut overheads = Vec::with_capacity(n);
        let mut segments = Vec::with_capacity(n);
        let mut assembles = Vec::with_capacity(n);
        let mut failed = 0usize;
        for task in tasks {
            match task.await {
                Ok(Ok(out)) => {
                    for event in &out.journal.events {
                        let EventKind::TurnFinished { timing, .. } = &event.kind else { continue };
                        overheads.push(timing.overhead_us());
                        segments.push(timing.segment_us);
                        assembles.push(timing.assemble_us);
                    }
                }
                _ => failed += 1,
            }
        }
        let wall = started.elapsed();

        // Every agent must have reached the shared adapter, or the row below is measuring a
        // thousand runs that quietly did nothing.
        let calls = provider.calls.load(Ordering::Relaxed);
        assert_eq!(calls, n as u64, "{calls} of {n} agents reached the provider");
        assert_eq!(failed, 0, "{failed} of {n} runs failed");

        overheads.sort_unstable();
        segments.sort_unstable();
        assembles.sort_unstable();
        let median = percentile(&overheads, 50);
        if n == 1 {
            baseline = median;
        }
        println!(
            "{n:>6}  {median:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
            percentile(&overheads, 99),
            percentile(&segments, 50),
            percentile(&assembles, 50),
            wall.as_millis(),
        );
    }

    println!(
        "\nRead the first column. Flat means Frey's own per-turn cost does not depend on how many\n\
         agents are running; rising means it does, and the segment/assemble columns say where.\n\
         Baseline at one agent was {baseline} µs.\n\n\
         Wall-clock is not a throughput figure — every run waits {} ms on a sleeping provider, so\n\
         the last column mostly measures how well the runtime overlaps that sleep.",
        LATENCY.as_millis()
    );
}
