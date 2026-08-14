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

Every turn emits `EventKind::TurnFinished` carrying a `TurnTiming`, into the journal **and** the
tracing span. Seven phases, five of them Frey's, two not, and a total.

**~12 µs of framework overhead per turn**, steady state, release build — the median of 1,024
concurrent runs. Against a real turn of one to three seconds of provider wait, that is around
**0.001%**.

### The first turn costs twenty times that, and it is worth saying separately

```
cargo run --release -p frey --example turn_timing     # one turn, cold process
```

reads about **280 µs**, of which roughly 95% is first-run cost: lazy initialisation, allocator
growth, first-touch page faults. That figure was published here for about an hour as "Frey's
per-turn overhead" before the concurrency sweep made it obviously wrong — one agent at 288 µs and a
thousand at 12 µs each reads as *"Frey gets cheaper under load"*, which is the tell. Recorded as
[I-013](../notes/INCIDENTS.md).

Both numbers are real. **~280 µs is what your first turn costs; ~12 µs is what every turn after it
costs.**

### The breakdown, cold, so the phases are visible at all

```
  segment           115 µs   frey        building segments from tools + history
  budget              7 µs   frey        deciding what to evict
  plan               35 µs   frey        breakpoints, churn, minimum prefix
  assemble           86 µs   frey        applying eviction, building + cloning the request
  account            37 µs   frey        decode, estimator reconciliation, events
  provider           15 µs   NOT frey    waiting for the model
  tools               0 µs   NOT frey    running the caller's code
  unaccounted         2 µs               nobody put a clock here
```

Warm, every one of those collapses to one or two microseconds. Segmentation and assembly are the
two that matter because they are the two that scale with prompt size.

### Read all of it narrowly

**It is the smallest possible prompt**: one user message, no tools. `build_segments` walks every
tool definition and every turn in the history, and assembly clones the whole request, so both grow
with the prompt. A 200-tool catalog over a 50-turn history is the case that actually matters for the
workload Frey is aimed at, and **it has not been measured.**

### Why overhead is computed by subtraction

`overhead_us` is `total - provider - tools`, not the sum of the instrumented phases. So time spent
in the loop that nobody thought to instrument lands **inside** the overhead figure and shows as
`unaccounted`, rather than disappearing from the report. A breakdown that always adds up is a
breakdown that cannot show you a surprise, and every entry in
[`notes/INCIDENTS.md`](../notes/INCIDENTS.md) is a surprise a measurement was defined not to have.

One bug found by building this: `provider.complete(request.clone())` evaluates the clone *before*
the future starts, so cloning the entire prompt — every turn — was billed to "waiting for the
provider". The clone is hoisted out now and counted as assembly, where it belongs.

## Concurrency

```
cargo run --release -p frey --example concurrency
```

N agents, **one shared provider adapter** behind an `Arc`, each provider call sleeping 50 ms to
stand in for network and inference so the runs genuinely overlap. Eight worker threads.

| agents | median overhead µs | p99 µs | segment | assemble | wall ms |
|---|---|---|---|---|---|
| 1 | 101 | 101 | 3 | 12 | 63 |
| 8 | 45 | 180 | 3 | 3 | 60 |
| 64 | 18 | 609 | 1 | 2 | 64 |
| 256 | 13 | 601 | 1 | 1 | 67 |
| 1024 | **11** | 591 | 1 | 1 | 72 |

**The median does not degrade from 1 to 1,024 concurrent agents.** It falls, because the low-N rows
have too few samples to be medians at all — the N=1 row *is* one measurement, and it moves by 3×
between repeats. The bottom row has 1,024 and is the one to trust.

**The p99 is the honest bad news.** It sits at 40–60× the median and is noisy across repeats
(400–2,900 µs at the same N on different runs). Some turn, somewhere, waits. The medians not moving
says it is scheduler jitter rather than contention, but that is an inference and not a measurement:
characterising it properly needs repeats and a histogram, which do not exist.

### What this cannot tell you

No sockets, no TLS, no HTTP/2 stream limits, no rate limits, no DNS. A flat median here means *Frey*
does not degrade with concurrency — not that your provider will not. Those are different claims and
only the first is being made.

The test that pins the *correctness* half — 64 agents, one adapter, distinct journals — is
`crates/frey/tests/concurrency.rs`, and it found [I-012](../notes/INCIDENTS.md) on its first run:
every run in a process shared one run id.

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
| Overhead on a realistic prompt (many tools, long history) | **Not measured.** The numbers above are the floor, not the figure. |
| Tail latency under concurrency | **Measured and not explained.** p99 is 40–60× the median and moves a lot between repeats. |
| Concurrency against a *real* provider | **Not measured.** The sweep uses a sleeping fake, so it says nothing about sockets, TLS or rate limits. |
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
