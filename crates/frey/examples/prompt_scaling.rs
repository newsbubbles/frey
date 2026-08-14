//! What Frey's per-turn overhead costs on a prompt that looks like real work.
//!
//! Run with `cargo run --release -p frey --example prompt_scaling`.
//!
//! Every other measurement in this repository uses the smallest prompt possible — one user message,
//! no tools — and reports around **12 µs** of framework overhead per turn. That is a floor, not a
//! figure, and saying so was not enough: the two phases that dominate are the two that grow with the
//! prompt. `build_segments` walks every tool definition and every turn in the history, and assembly
//! clones the request. So the honest question is not "what does a turn cost" but "what does a turn
//! cost **on a 200-tool catalog over a 50-turn conversation**", which is the shape this framework is
//! aimed at.
//!
//! Two sweeps, because there are two axes and they are worth separating:
//!
//! 1. **Tools.** One turn, catalogs from 0 to 500 tools.
//! 2. **History.** A fixed 200-tool catalog, one agentic run of 50 turns, reporting each turn — so
//!    the history grows through the real loop rather than being fabricated. Frey has no API to seed
//!    a conversation, and building one for a benchmark would measure a code path nobody uses.
//!
//! Warmed up before measuring, and the catalog sweep takes the median of nine runs. The first run
//! in a process costs about twenty times the steady-state figure, and a sweep that forgets that
//! reads as "it gets faster with more tools".
//!
//! # What it found
//!
//! **The catalog is the cost and the conversation is not.** Overhead is close to linear in the
//! number of tools at roughly **16 µs per tool per turn** — about 3.3 ms on a 200-tool catalog,
//! stable across repeats — while twenty-five turns of accumulated history moved it not at all.
//! A tool catalog is re-segmented, re-hashed and re-cloned on **every** turn; history grows slowly
//! by comparison and the budgeter is already evicting it.
//!
//! Roughly half of that is `assemble`, which is dominated by cloning every tool definition into
//! the request once per turn. That is an obvious thing to fix — the definitions do not change
//! within a run — and it is measured rather than assumed now.
//!
//! It is also the sharpest argument for progressive disclosure, which this repository has built and
//! [not wired into the loop](../../../README.md). The cost of handing a model 200 tools it will not
//! use is no longer a design opinion; it is 3.3 ms a turn.

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

/// Catalog sizes for the first sweep. 200 is the number people actually hit once a few MCP servers
/// are connected; 500 is there to show the shape of the curve rather than to be realistic.
const CATALOGS: &[usize] = &[0, 10, 50, 200, 500];

/// How deep the second sweep lets the conversation get, counted in *items of history* rather than
/// turns — the loop appends an assistant turn and a tool-result turn per round, so this is about
/// twice the number of turns actually measured.
const CONVERSATION_TURNS: u32 = 50;

/// Roughly the size of a real tool result — a file listing, an API response, a search hit.
const RESULT_BYTES: usize = 400;

/// A provider that keeps the conversation going until the history is deep enough.
///
/// Stateless: it decides from `request.turns.len()`, which is already in front of it, rather than
/// keeping a counter. That also means it is driven by the same history the loop is measuring.
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

        // Frey's own estimator, applied to what actually arrived, so the report can put overhead
        // against prompt size rather than against a turn index that means nothing on its own.
        let bytes: usize = request
            .turns
            .iter()
            .flat_map(|t| t.items.iter())
            .map(|i| match i {
                Item::Text(t) => t.text.len(),
                Item::ToolResult(r) => r.content.len(),
                _ => 0,
            })
            .sum::<usize>()
            + request
                .tools
                .iter()
                .map(|t| t.name.as_str().len() + t.description.len())
                .sum::<usize>();
        let input = u64::try_from(bytes / 4).unwrap_or(u64::MAX);

        // `StopReason::ToolUse`, not `EndTurn`. The loop returns on `EndTurn` even when tool calls
        // are present — correctly, since a model saying it is finished is finished — so the first
        // version of this ended every conversation at turn 0 and the history sweep measured one
        // turn. A benchmark that silently measures a tenth of what it says it does.
        let (items, stop) = if request.turns.len() < self.depth {
            (
                vec![Item::ToolCall(ToolCallItem {
                    id: CallId::new(format!("call-{n}")),
                    name: ToolName::new("tool_000"),
                    args: serde_json::json!({"query": "something plausible", "limit": 20}),
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
            usage: Usage { input, output: 8, ..Usage::default() },
            stop,
            model: request.model,
            provider: self.id(),
        })
    }

    async fn stream(&self, _request: Request) -> Result<EventStream, ProviderError> {
        Err(ProviderError::Unsupported { provider: self.id(), capability: "streaming".into() })
    }
}

/// A catalog of `n` tools with descriptions and schemas of realistic weight.
///
/// Not `n` copies of a stub: a real catalog's cost is in its *text*, which is what gets segmented,
/// hashed for churn detection, and cloned into every request.
struct Catalog {
    tools: Vec<ToolDefinition>,
}

impl Catalog {
    fn of(n: usize) -> Self {
        let tools = (0..n)
            .map(|i| {
                ToolDefinition::new(
                    format!("tool_{i:03}"),
                    format!(
                        "Search the {i} index for records matching a query and return the top \
                         results with their identifiers, scores and a short excerpt of the matched \
                         text. Use this before attempting to modify anything in index {i}."
                    ),
                    JsonSchema::new(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Free-text query. Supports quoted phrases."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "How many results to return, 1 to 100.",
                                "minimum": 1,
                                "maximum": 100
                            }
                        },
                        "required": ["query"]
                    }))
                    .expect("the schema in this example is valid"),
                )
            })
            .collect();
        Self { tools }
    }
}

impl ToolHost for Catalog {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>, ToolError> {
        Ok(self.tools.clone())
    }

    async fn call(
        &self,
        _invocation: Invocation,
        _cx: &ToolCx,
    ) -> ToolOutcome<frey_core::tool::ToolValue> {
        ToolOutcome::Ok(Tainted::with_provenance(
            ToolContent::text("r".repeat(RESULT_BYTES)),
            Provenance::new("tool:search"),
        ))
    }
}

struct Turnwise {
    turn: u32,
    prompt_tokens: u64,
    overhead_us: u64,
    segment_us: u64,
    assemble_us: u64,
}

async fn measure(tools: usize, depth: usize, max_turns: u32) -> Vec<Turnwise> {
    let provider = Arc::new(Chatty { depth, calls: AtomicU64::new(0) });
    let agent = Agent::new(provider, Catalog::of(tools), "chatty-model").max_turns(max_turns);
    let Ok(out) = agent.run("begin").await else { return Vec::new() };

    // `UsageUpdated` lands before `TurnFinished` in the same turn, so the most recent one is this
    // turn's prompt size.
    let mut latest_input = 0;
    let mut rows = Vec::new();
    for event in &out.journal.events {
        match &event.kind {
            EventKind::UsageUpdated { usage } => latest_input = usage.input,
            EventKind::TurnFinished { turn, timing } => rows.push(Turnwise {
                turn: turn.0,
                prompt_tokens: latest_input,
                overhead_us: timing.overhead_us(),
                segment_us: timing.segment_us,
                assemble_us: timing.assemble_us,
            }),
            _ => {}
        }
    }
    rows
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    for _ in 0..32 {
        let _ = measure(10, 0, 1).await;
    }

    println!("== overhead against catalog size (one turn) ==\n");
    println!(
        "{:>7}  {:>14}  {:>12}  {:>10}  {:>10}",
        "tools", "prompt tokens", "overhead µs", "segment", "assemble"
    );
    println!("{:->7}  {:->14}  {:->12}  {:->10}  {:->10}", "", "", "", "", "");
    for &n in CATALOGS {
        // Several runs and take the median: one sample is one sample, which is the mistake that
        // put a cold-start number in the documentation earlier today.
        let mut samples: Vec<(u64, u64, u64, u64)> = Vec::new();
        for _ in 0..9 {
            for row in measure(n, 0, 1).await {
                samples.push((row.prompt_tokens, row.overhead_us, row.segment_us, row.assemble_us));
            }
        }
        samples.sort_unstable_by_key(|s| s.1);
        let mid = samples[samples.len() / 2];
        println!("{n:>7}  {:>14}  {:>12}  {:>10}  {:>10}", mid.0, mid.1, mid.2, mid.3);
    }
    println!("\n== overhead against history (200 tools, one {CONVERSATION_TURNS}-turn run) ==\n");
    println!(
        "{:>6}  {:>14}  {:>12}  {:>10}  {:>10}",
        "turn", "prompt tokens", "overhead µs", "segment", "assemble"
    );
    println!("{:->6}  {:->14}  {:->12}  {:->10}  {:->10}", "", "", "", "", "");
    let rows = measure(200, CONVERSATION_TURNS as usize, CONVERSATION_TURNS + 1).await;
    for row in rows.iter().step_by(5) {
        println!(
            "{:>6}  {:>14}  {:>12}  {:>10}  {:>10}",
            row.turn, row.prompt_tokens, row.overhead_us, row.segment_us, row.assemble_us
        );
    }
    if let (Some(a), Some(b)) = (rows.first(), rows.last()) {
        println!(
            "\n  turn {} -> turn {}: prompt {} -> {} tokens, overhead {} -> {} µs.",
            a.turn, b.turn, a.prompt_tokens, b.prompt_tokens, a.overhead_us, b.overhead_us
        );
        let total: u64 = rows.iter().map(|r| r.overhead_us).sum();
        println!(
            "  whole {}-turn conversation: {} µs of framework overhead in total.",
            rows.len(),
            total
        );
    }
}
