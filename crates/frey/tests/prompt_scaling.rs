//! Which part of a prompt costs Frey time.
//!
//! The `prompt_scaling` example produces the numbers; this pins the **shape**, which is what a
//! regression would change. Timing assertions flake, so every margin here is deliberately huge —
//! the measured effect is roughly 800x and this test asks for 10x. A margin that only just passes
//! on the author's machine is a test that fails on somebody else's CI for no reason, and a flaky
//! performance test gets deleted rather than investigated.
//!
//! What it would catch: segmentation or assembly becoming quadratic in history, or the tool catalog
//! quietly ceasing to be the thing that dominates — which would mean the documented advice to worry
//! about the catalog rather than the conversation had stopped being true.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use frey::prelude::*;
use frey_core::event::EventKind;
use frey_core::ids::{CallId, ModelId, ProviderId, ToolName};
use frey_core::item::{Caller, TextItem, ToolCallItem};
use frey_core::provider::{
    EventStream, ModelProvider, ProviderError, Request, Response, StopReason,
};
use frey_core::provider_caps::ProviderCapabilities;
use frey_core::taint::Provenance;
use frey_core::usage::Usage;

const BIG_CATALOG: usize = 200;
const HISTORY_DEPTH: usize = 30;

struct Chatty {
    depth: usize,
    calls: AtomicU64,
}

impl ModelProvider for Chatty {
    fn id(&self) -> ProviderId {
        ProviderId::new("chatty")
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities::minimal(1_000_000, 4_096)
    }

    async fn complete(&self, request: Request) -> Result<Response, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        let (items, stop) = if request.turns.len() < self.depth {
            (
                vec![Item::ToolCall(ToolCallItem {
                    id: CallId::new(format!("call-{n}")),
                    name: ToolName::new("tool_000"),
                    args: serde_json::json!({"query": "q"}),
                    caller: Caller::Direct,
                })],
                StopReason::ToolUse,
            )
        } else {
            (
                vec![Item::Text(TextItem {
                    text: "done".into(),
                    provenance: Some(Provenance::new("provider:chatty")),
                })],
                StopReason::EndTurn,
            )
        };
        Ok(Response {
            items,
            usage: Usage { input: 100, output: 8, ..Usage::default() },
            stop,
            model: request.model,
            provider: self.id(),
        })
    }

    async fn stream(&self, _request: Request) -> Result<EventStream, ProviderError> {
        Err(ProviderError::Unsupported { provider: self.id(), capability: "streaming".into() })
    }
}

struct Catalog(Vec<ToolDefinition>);

impl Catalog {
    fn of(n: usize) -> Self {
        Self(
            (0..n)
                .map(|i| {
                    ToolDefinition::new(
                        format!("tool_{i:03}"),
                        format!(
                            "Search the {i} index for records matching a query and return the top \
                             results with identifiers, scores and an excerpt of the matched text."
                        ),
                        JsonSchema::new(serde_json::json!({
                            "type": "object",
                            "properties": {"query": {"type": "string"}},
                            "required": ["query"]
                        }))
                        .expect("valid schema"),
                    )
                })
                .collect(),
        )
    }
}

impl ToolHost for Catalog {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>, ToolError> {
        Ok(self.0.clone())
    }

    async fn call(
        &self,
        _invocation: Invocation,
        _cx: &ToolCx,
    ) -> ToolOutcome<frey_core::tool::ToolValue> {
        ToolOutcome::Ok(Tainted::with_provenance(
            ToolContent::text("r".repeat(400)),
            Provenance::new("tool:search"),
        ))
    }
}

async fn overheads(tools: usize, depth: usize, max_turns: u32) -> Vec<u64> {
    let provider = Arc::new(Chatty { depth, calls: AtomicU64::new(0) });
    let agent = Agent::new(provider, Catalog::of(tools), "chatty").max_turns(max_turns);
    let Ok(out) = agent.run("begin").await else { return Vec::new() };
    out.journal
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::TurnFinished { timing, .. } => Some(timing.overhead_us()),
            _ => None,
        })
        .collect()
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
async fn overhead_tracks_the_catalog_and_not_the_history() {
    // Warm up. The first run in a process costs roughly twenty times steady state, and measuring
    // the empty catalog first would hand it the whole cold-start bill and invert the result.
    for _ in 0..16 {
        let _ = overheads(4, 0, 1).await;
    }

    let mut none = Vec::new();
    let mut many = Vec::new();
    for _ in 0..5 {
        none.extend(overheads(0, 0, 1).await);
        many.extend(overheads(BIG_CATALOG, 0, 1).await);
    }
    let none = median(none);
    let many = median(many);

    // Measured at ~800x. Asking for 10x leaves room for a slow, loaded, or virtualised machine
    // while still failing loudly if the catalog stops being the thing that costs.
    assert!(
        many > none.saturating_mul(10),
        "a {BIG_CATALOG}-tool catalog should cost far more per turn than none: {many} vs {none} µs"
    );

    // And the other half of the claim: history does not pile up. Measured flat across 25 turns.
    // The assertion allows the last turn to be 5x the first, which a linear-in-history cost over
    // this many turns would break and ordinary jitter will not.
    let run = overheads(BIG_CATALOG, HISTORY_DEPTH, 64).await;
    assert!(run.len() >= 8, "the conversation must actually have run: {} turns", run.len());
    let first = run[0].max(1);
    let last = run[run.len() - 1];
    assert!(
        last < first.saturating_mul(5),
        "overhead must not grow with conversation length: turn 0 was {first} µs and turn {} was \
         {last} µs",
        run.len() - 1
    );
}
