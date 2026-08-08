# Research 05 — Rust Building Blocks & Idioms

*Gathered 2026-08-08. Decisions here are load-bearing: they set what the public API can look like
for the next few years, and public traits are the hardest thing to change later.*

---

## 1. The async-trait question (decide first, everything hangs off it)

Facts as of 2026:
- AFIT (`async fn` in traits) stable since **1.75** — but **not `dyn`-compatible**.
  `dyn Provider` where `Provider` has `async fn` still does not work on stable.
- `async-trait`: works, but rewrites every method to
  `Pin<Box<dyn Future + Send + 'async_trait>>` → **heap allocation per call, inlining killed**.
- `dynosaur`: proc macro that generates a `dyn`-compatible erased wrapper **while leaving the
  native trait statically dispatched**. You pay boxing only where you actually erase.
- `trait-variant`: emits `Send` and non-`Send` variants of a trait from one definition.

**Decision for Frey:** define traits **natively with AFIT** (`async fn` / RPITIT), and generate
erased counterparts with **`dynosaur`** for the places that genuinely need `dyn`
(the provider registry, the tool registry, plugin boundaries). Use `trait-variant` where a
non-`Send` variant is worth offering (wasm32 targets — Rig's wasm support proves people want this).

Rationale: an agent loop makes O(10) provider calls and O(100) tool calls per run; a box per call
is *irrelevant* to performance, but a boxed future in the **hot streaming path** (per-token
`poll_next`) is not. So: erase at the coarse boundary, keep the fine-grained path static.

> Write this in an ADR. It is the kind of decision a contributor will otherwise "fix" wrongly.

---

## 2. Tool middleware = `tower`

`pydantic-ai`'s twelve toolset classes (Filtered / Prefixed / Renamed / Prepared / Wrapper /
ApprovalRequired / DeferredLoading / IncludeReturnSchemas / SetMetadata / Combined / External /
Function) are all one thing: **middleware over a tool service.**

```rust
// The shape everything reduces to:
//   Service<ToolInvocation, Response = ToolOutcome, Error = ToolError>
// composed with tower::Layer.
```

Layers Frey should ship (each a small, separately testable crate module):

| Layer | Does |
|---|---|
| `NamespaceLayer` | prefix / rename, collision detection |
| `FilterLayer` | per-run, per-step visibility predicate |
| `PrepareLayer` | rewrite `ToolDefinition`s before presentation (enrich descriptions, tighten schemas) |
| `ApprovalLayer` | HITL gate; emits `input_required`, displays the **literal** action |
| `PolicyLayer` | capability check, Rule-of-Two enforcement, taint check |
| `SandboxLayer` | route execution into a sandbox backend |
| `TimeoutLayer` / `ConcurrencyLimitLayer` / `RateLimitLayer` | straight from `tower` |
| `RetryLayer` | with the **error taxonomy** from research 02 §6, not blind retries |
| `CacheLayer` | memoise pure tools by (name, args) hash |
| `AuditLayer` | append-only record + OTel span |
| `RedactLayer` | strip secrets from args and results before they are logged *or* returned |

`tower` gives us `Layer`, `ServiceBuilder`, `Steer`, load-shedding, and a decade of production use.
This is the single clearest "Rust is structurally better here" argument in the whole design, and it
costs us nothing to adopt.

---

## 3. Schemas

- **`schemars`** generates **JSON Schema draft 2020-12** — exactly what MCP 2026-07-28 now allows
  for `inputSchema` / `outputSchema` (it loosened to "any 2020-12 keywords" and added `$ref`
  resolution requirements). Alignment is free.
- **`serde` + `serde_json`** for the wire; keep a `RawValue` escape hatch so `Opaque` items
  round-trip byte-for-byte (research 02 §5 rule 1).
- Validation of *model output* against the schema needs a runtime validator: **`jsonschema`** crate.
  Needed because "strict mode" is provider-dependent (Responses attempts strict and silently falls
  back; Anthropic's grammar mode builds from the full toolset) — so Frey must be able to validate
  itself and produce a **model-directed** error (R9) when the model violates a schema.
- Derive ergonomics target:

```rust
#[frey::tool(
    description = "Read a file from the workspace",
    caller = "code|direct",
    capabilities("fs:read"),
)]
async fn read_file(
    ctx: &Ctx,
    /// Path relative to the workspace root.
    path: WorkspacePath,
) -> Result<Tainted<String, LowIntegrity>, ToolError> { … }
```
Doc comments on parameters become `description` fields — this matters because
**tool search matches on argument names and argument descriptions** (research 01 §3.1).
Frey should *lint* tools whose params lack docs, because that directly degrades discoverability.

---

## 4. Observability

- **`tracing`** + `tracing-subscriber` for structured events; **`tracing-opentelemetry`** to export.
- MCP 2026-07-28 **standardised OTel context propagation in `_meta`** (`traceparent`, `tracestate`,
  `baggage`). So: inject on outbound MCP calls, extract on inbound. A Frey MCP **server** and a
  Frey **client** in the same trace is a demo that sells the framework.
- Fill the surveyed gap on **span correlation across sub-agent boundaries**: a sub-agent run must
  attach to the parent span, whether it runs in-process, in another task, or as a child process
  (propagate via env var / `_meta`, same as HTTP).
- Semantic conventions: OTel has `gen_ai.*` attributes. Use them rather than inventing names —
  it means existing dashboards (Langfuse, Phoenix, Braintrust, Grafana) light up for free.
  Custom `frey.*` attributes only for things with no convention (cache plan, context budget,
  taint labels, capability decisions).

---

## 5. Determinism, replay, durability

Nobody in Rust has first-class deterministic agent replay (research 03 §1, gap 4). Cheapest
credible path:

1. **Event-sourced run journal.** Every non-deterministic effect — model response, tool result,
   clock read, RNG draw, MCP list result — is appended to a journal with a monotonic sequence id.
2. **Replay mode** feeds the journal back instead of performing the effect; any divergence in the
   *request* fingerprint is a hard error pointing at the exact step.
3. That single mechanism buys: regression tests over real transcripts, `frey replay` for debugging,
   crash resumption, and cost-free "what if I change the prompt" A/B on recorded runs.
4. Only *then* consider adapters to **Temporal** (`temporalio-sdk`; note its deterministic
   `select!`/`join!` wrappers — ordinary tokio combinators break replay), **Restate**
   (`restate-sdk`, execution log), or WASM-based engines (`flawless`, `durable`).
   Do **not** take a durable-execution engine as a core dependency in v1.

Also adopt **deterministic simulation testing** for the loop itself (seeded scheduler, simulated
clock, injected failures) — this is well-trodden in Rust and it is how we make "1,000 runs, zero
flakes" a claim we can actually back.

---

## 6. Crate hygiene / shape

- Workspace with **many small crates**, one facade (`frey`) with feature flags — Rig's
  `rig-core`/`rig-agent`/`rig` facade is the right shape; ADK-Rust's **22 crates** is the
  cautionary tale ("complexity of maintaining 22 crates; steep learning curve"). Target **~10**.
- MSRV policy stated in the README; `rust-version` in every manifest.
- `#![forbid(unsafe_code)]` everywhere except the sandbox backends, which isolate `unsafe`
  in a small documented module.
- `cargo deny` + `cargo audit` in CI; SBOM on release.
- Every public example in `examples/` must be a **compiled doctest or an integration test**, because
  R13 (coding agents must be able to use this) fails the instant a doc example goes stale.
- **Publish the config JSON Schema** (`frey.schema.json`) so an agent can author `frey.toml`
  correctly, and so editors validate it.

---

## 7. Runtime hazards to design around (from the surveys)

- **Long CPU-bound work stalls the Tokio scheduler.** Anything that could block — schema
  compilation, BM25 indexing, embedding, sandbox setup, large JSON parsing — goes through
  `spawn_blocking` or yields explicitly. Frey should ship a `blocking!` helper and lint for it.
- **Actors forbid shared read-only context**, yet agents mostly want exactly that. Use
  `Arc<Ctx>` + `arc-swap` for hot-reloadable shared config rather than an actor mailbox.
  (This is the second named ecosystem gap; solving it is just picking the right primitive.)
- **Streaming through agent trees**: leaf tokens must reach the root without buffering at each
  layer. `tokio_stream` + a broadcast/bounded-mpsc fan-in, with backpressure that drops
  *presentation* events (deltas) before it drops *semantic* events (tool calls, errors).
  This maps 1:1 onto AG-UI's event stream, so build the internal event bus **as** the AG-UI
  event model and get the frontend protocol for free.

---

## Sources

- [dynosaur](https://docs.rs/dynosaur/latest/dynosaur/) · [async-trait](https://docs.rs/async-trait) · [Dyn Async Traits (baby steps)](https://smallcultfollowing.com/babysteps/series/dyn-async-traits/) · [The Async Trait Problem: What Finally Works in 2026](https://wrenlearnsrust.com/posts/async-traits-2026.html)
- [schemars](https://github.com/GREsau/schemars) · [schemars docs](https://graham.cool/schemars/) · [jsonschema crate](https://crates.io/crates/jsonschema)
- [temporalio-sdk](https://crates.io/crates/temporalio-sdk) · [restate-sdk](https://docs.rs/restate-sdk/latest/restate_sdk/) · [flawless](https://crates.io/crates/flawless) · [iopsystems/durable](https://github.com/iopsystems/durable)
- [Deterministic Simulation Testing in Rust (Polar Signals)](https://www.polarsignals.com/blog/posts/2025/07/08/dst-rust)
