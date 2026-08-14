//! Many agents, one provider adapter.
//!
//! The `concurrency` example measures how this *performs*; this pins that it is **correct**, which
//! is the part that can regress silently. Small N so it stays a test rather than a benchmark.
//!
//! What could go wrong and would not show up anywhere else: agents sharing a journal, a run's
//! timing landing on another run's record, or the shared-adapter pattern simply not compiling —
//! which is not hypothetical. Until 0.2.0, `Arc<P>` was not a `ModelProvider`, so the pattern
//! documented in three places did not build, and the obvious way out is one adapter per agent,
//! which is the exact failure those docs warn about.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

const AGENTS: usize = 64;

/// Answers from `&self` with no interior mutability, so nothing here serialises the agents and the
/// test measures Frey rather than its own lock.
struct SharedModel {
    calls: AtomicU64,
}

impl ModelProvider for SharedModel {
    fn id(&self) -> ProviderId {
        ProviderId::new("shared")
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities::minimal(200_000, 4_096)
    }

    async fn complete(&self, request: Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // A yield rather than a sleep: enough for the runtime to interleave the tasks, without
        // making the test's duration a function of a timer.
        tokio::task::yield_now().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn many_agents_share_one_adapter_and_keep_their_records_apart() {
    let provider = Arc::new(SharedModel { calls: AtomicU64::new(0) });

    let mut tasks = Vec::with_capacity(AGENTS);
    for _ in 0..AGENTS {
        let shared = Arc::clone(&provider);
        tasks.push(tokio::spawn(async move {
            Agent::new(shared, NoTools, "shared-model").run("go").await
        }));
    }

    let mut runs = Vec::with_capacity(AGENTS);
    for task in tasks {
        runs.push(task.await.expect("no task panicked").expect("every run succeeded"));
    }

    assert_eq!(
        provider.calls.load(Ordering::Relaxed),
        AGENTS as u64,
        "every agent must actually have reached the shared adapter"
    );

    // **Distinct run ids.** A shared journal would be the worst possible bug here: every assertion
    // about cost, replay and incident attribution rests on one run's record being one run's record.
    let ids: std::collections::HashSet<_> = runs.iter().map(|r| r.journal.run.clone()).collect();
    assert_eq!(ids.len(), AGENTS, "runs must not share a journal");

    for run in &runs {
        let timings: Vec<_> = run
            .journal
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::TurnFinished { timing, .. } => Some(*timing),
                _ => None,
            })
            .collect();
        assert_eq!(timings.len(), 1, "one turn, one timing, per run");
        let t = timings[0];
        assert!(t.total_us > 0);
        assert!(
            t.overhead_us() <= t.total_us,
            "concurrency must not make the framework's share exceed the turn: {t:?}"
        );
        assert!(
            t.accounted_us() <= t.overhead_us(),
            "the phase breakdown must stay a breakdown under load: {t:?}"
        );
    }
}
