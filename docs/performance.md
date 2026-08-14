# Performance

Frey had no performance numbers at all until 2026-08-14, and `claims.toml` had no performance rows —
which at least meant nothing was overclaimed. This page is what exists now, and it is deliberately
short, because most of what people want to know is still unmeasured.

## The rule this page follows

**A framework reporting one undivided latency number is reporting mostly the network.** The first
thing measured here was MCP startup, and 99.5% of it turned out to be somebody else's process
starting up — a figure that says nothing about any framework, in any language. So every number below
separates what Frey does from what Frey waits for.

## Per-turn overhead

Every turn emits [`EventKind::TurnFinished`] carrying a `TurnTiming`, into the journal **and** the
tracing span. Seven phases, three of them Frey's, and a total:

```
cargo run --release -p frey --example turn_timing
```

```
  segment           115 µs   frey        building segments from tools + history
  budget              7 µs   frey        deciding what to evict
  plan               35 µs   frey        breakpoints, churn, minimum prefix
  assemble           86 µs   frey        applying eviction, building + cloning the request
  account            37 µs   frey        decode, estimator reconciliation, events
  provider           15 µs   NOT frey    waiting for the model
  tools               0 µs   NOT frey    running the caller's code
  unaccounted         2 µs               nobody put a clock here
  ----------------------------------------
  OVERHEAD          282 µs   frey's share
  turn total        297 µs
```

**~282 µs**, release build, against a real turn that takes one to three seconds of provider wait.
That is roughly **0.03% of a typical turn** — and roughly 0.0008% of the eight seconds it takes to
start one MCP server.

### Read that number narrowly

**It is the smallest possible prompt**: one user message, no tools, one turn. The two dominant
phases both scale with prompt size — `build_segments` walks every tool definition and every turn in
the history, and assembly clones the whole request. A 200-tool catalog over a 50-turn history is the
case that actually matters for the workload Frey is aimed at, and **it has not been measured.**
`claims.toml` records this as `tested-only` for exactly that reason. Quoting 0.3 ms as *the* number
for a real agent would be quoting the easiest case available.

### Why overhead is computed by subtraction

`overhead_us` is `total - provider - tools`, not the sum of the instrumented phases. So time spent
in the loop that nobody thought to instrument lands **inside** the overhead figure and shows as
`unaccounted`, rather than disappearing from the report. A breakdown that always adds up is a
breakdown that cannot show you a surprise, and every entry in
[`notes/INCIDENTS.md`](../notes/INCIDENTS.md) is a surprise a measurement was defined not to have.

One bug found by building this: `provider.complete(request.clone())` evaluates the clone *before*
the future starts, so cloning the entire prompt — every turn — was billed to "waiting for the
provider". The clone is hoisted out now and counted as assembly, where it belongs.

## Reading it back from a real run

```bash
frey timings path/to/journal.jsonl
```

Medians rather than means, because one turn where a provider queued for thirty seconds drags a mean
far enough to hide what the other ninety turns did. Add `--json` for a machine-readable object.

A journal written before `TurnTiming` existed reports **no data and exits non-zero** rather than
printing zeros, because a zero here would be an invented measurement.

## What is not measured

| | State |
|---|---|
| Overhead on a realistic prompt (many tools, long history) | **Not measured.** The number above is the floor, not the figure. |
| Concurrency — many agents on one shared adapter | **Not measured.** `complete(&self)`, `Arc<P>: ModelProvider` and `HttpProvider::with_client` exist for it; no load test does. |
| Throughput, memory, allocation counts | Not measured. |
| Comparison against Rig, pydantic-ai or anything else | Not measured, and not claimed anywhere. |
| MCP client connect cost | Not measurable yet — the client [ships no transport](../notes/INCIDENTS.md). |

## MCP startup, since it is the question people ask

Two full sweeps of ten third-party stdio servers, an hour apart:

| | before any server says a word | protocol round trips |
|---|---|---|
| run A | 73,090 ms | 102 ms |
| run B | 33,740 ms | 150 ms |

The absolute numbers moved by more than 2× between two runs of identical code on one machine,
because `npx -y` resolution and interpreter startup depend on what is warm. **Anything quoting a
single figure here is quoting the state of a cache.** The ratio is stable: >99.5% of the time to a
tool catalog is process startup.

So **the stateless revision is not a latency feature.** Skipping the `initialize` handshake saves
one round trip out of ~150 ms total, against tens of seconds of process startup. It is a *scaling*
property — any replica serves any request, no session affinity — and selling it as speed would be
selling a rounding error.

The real MCP performance question is not how fast the handshake is but how often you pay four to
nine seconds to start the process at all. Frey has no server reuse or connection pooling, so it has
no answer to that one either.

Regenerate with `cargo xtask conformance`; it costs nothing and involves no inference.
