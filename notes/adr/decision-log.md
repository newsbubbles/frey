# Frey — Decision Log

Status values: **Accepted** (do it), **Proposed** (leaning, needs a spike), **Open** (unresolved).
Each entry: decision → why → what it costs → how we'd know we were wrong.

---

## ADR-0001 — Target MCP `2026-07-28` natively, shim older revisions — **Accepted**

The current revision removes sessions and the handshake entirely; a framework built on the 2025
mental model is born legacy. Frey's MCP client is a **catalog cache over a stateless transport**,
not a session manager.

*Costs:* a compatibility shim (probe `server/discover`, fall back to `initialize`) and a
normalisation layer over two protocol shapes.
*Wrong if:* the ecosystem stalls on 2025-11-25 for years and the shim becomes the main path.
Mitigation: the shim is a separate module with its own test corpus either way.

## ADR-0002 — Build on `rmcp`, but own our domain types — **Accepted**

**Verified 2026-08-08 against docs.rs:** `rmcp` **3.1.2** (published 2026-08-07) exposes
`InputRequiredResult`, `ResultType`, `DiscoverResult`, `SubscriptionsListenRequest`, `CacheScope`,
and a feature-gated `RequestStateCodec` (HMAC-SHA256-sealed `requestState`). It implements
2026-07-28 while remaining compatible with 2025-11-25 and earlier. No standalone `CacheableResult`
struct was visible — assume the caching hints live on the individual list results; confirm in source.

So: depend on `rmcp` for the wire, and define Frey's own `ToolDefinition` / `ToolCatalog` /
`NeedsInput` so native tools, MCP tools, skills, and sub-agents are indistinguishable to the loop
and so SDK churn doesn't reach our public API.

*Wrong if:* `rmcp` turns out to be materially incomplete for streamable-HTTP servers under load —
in which case we implement the transport ourselves and keep the types.

## ADR-0003 — The conversation is a list of **Items**, not messages — **Accepted**

OpenAI's Responses API is item-based; Anthropic's blocks are item-like; reasoning state must be
round-tripped (`encrypted_content`) or the model gets dumber and more expensive. A message-shaped
core forces lossy conversion at exactly the place lossiness costs money.

*Enforced by:* a round-trip conformance test per provider (`raw → Vec<Item> → raw` byte-identical),
with `Opaque` preserving anything we don't model.
*Costs:* more types, and message-shaped providers need a projection layer.

## ADR-0004 — Two provider traits: `ModelProvider` and `AgentProvider` — **Accepted**

Riding a user's existing subscription is a real requirement (R4) but the ToS reality is asymmetric:
Anthropic **prohibits** third-party use of subscription OAuth (policy updated 2026-02-20), routing
sanctioned programmatic use through API keys or the Agent SDK credit pool; OpenAI's Codex path is
semi-official and personal-use-only, with account pooling explicitly discouraged.

Therefore Frey **never stores, mints, or replays a vendor subscription OAuth token.** The
subscription story is delegation to the vendor's own binary (`claude`, `codex`) as a child process
that keeps its own auth. This is ToS-clean, survives vendor auth changes, inherits their sandboxing,
and — usefully — makes "use Claude Code as a sub-agent" a one-liner.

*Costs:* we cannot offer token-level control over delegated agents. Say so in the docs.

## ADR-0005 — Tool plumbing is `tower` middleware — **Accepted**

pydantic-ai needed twelve `*Toolset` classes to express filtering, renaming, prefixing,
preparation, approval, deferral, metadata and wrapping. All twelve are middleware over
`Service<Invocation> -> ToolOutcome`. Rust already has `Layer`/`ServiceBuilder`, with retry,
timeout, concurrency and load-shed for free.

*Costs:* `tower`'s ergonomics are famously sharp; we must ship a `ToolStack::production()`
preset so nobody assembles a stack by hand for the common case.

## ADR-0006 — Async traits: native AFIT + `dynosaur` for erasure — **Accepted**

AFIT is stable but not `dyn`-compatible. `async-trait` boxes *every* call. `dynosaur` erases only
where erasure is needed. We erase at coarse boundaries (provider registry, tool registry, plugins)
and keep the per-token streaming path statically dispatched.

*Wrong if:* `dynosaur` proves immature for our trait shapes — fallback is hand-written
`Pin<Box<dyn Future>>` erasure wrappers, which is what `dynosaur` generates anyway.

## ADR-0007 — Context engine is a first-class crate, and the cache planner is a pure function — **Accepted**

`CachePlanner::plan(&catalog, &history, &caps) -> CachePlan` with no I/O means every provider
quirk (≤4 breakpoints, 1h-before-5m ordering, per-model minimum prefix of 512/1,024/2,048/4,096,
`tools→system→messages` invalidation cascade, OpenAI's `prompt_cache_key` ~15 rpm guidance,
OpenRouter's per-provider explicit-vs-automatic split) is a unit test rather than a production
surprise.

Headline behaviour: **refuse to place a breakpoint on a segment whose hash changed last turn**,
and emit a warning naming the culprit. This is the most immediately felt feature in the framework.

## ADR-0008 — Discovery delegates to the provider when it can, emulates when it can't — **Accepted**

Anthropic ships server-side tool search (regex + BM25, `defer_loading`, ≤5 results, 10k deferred
tools max, and a documented **custom** hook: return `tool_reference` blocks from an ordinary
`tool_result`). OpenAI's Responses API has its own hosted tool search. OpenRouter's routed models
have neither.

Frey exposes one `CapabilitySearch` trait (Regex / BM25 / Embedding) and one set of events, and
routes to the native implementation when `ProviderCapabilities::tool_search == Native`.
When emulating, discovered definitions are injected **after** the cache breakpoint so the stable
prefix is untouched — the same trick the API uses internally.

*Note:* deferred tools still have to be **transmitted** on every Anthropic request; the saving is
context, not bandwidth. Don't overclaim in the README.

## ADR-0009 — Errors are typed by audience — **Accepted**

`ToolError { model: ModelMessage { summary, guidance, suggested_tools, schema_hint },
operator: Diagnostic, user: Option<Presentation>, retry: RetryDirective }`, and
`ToolOutcome::{Ok, Failed, Denied, NeedsInput}`.

Precedent: pydantic-ai distinguishes `ModelRetry` (consumes retry budget) from `ToolFailed`
(doesn't). We add `Denied` because permission refusal must reach the model *and* alert the operator.
Provider-side, `Auth` and `Billing` are non-retryable and loud — silent OpenRouter 402 degradation
is a verified real-world failure mode.

## ADR-0010 — One `NeedsInput` type for approvals, MRTR, AG-UI interrupts, and suspension — **Accepted**

MCP's `input_required` / `inputRequests` / `inputResponses` retry, AG-UI's interrupt
(pause/approve/edit/retry/escalate), human approval gates, frontend-executed tools, and durable
suspension are the same shape. Unifying them collapses four subsystems into one code path and
makes durable execution a later adapter rather than a rewrite.

## ADR-0011 — Security: capabilities + typed taint + fail-closed sandbox — **Accepted**

- **Capabilities** declared per tool; no ambient authority; `Secret` never materialises inside a
  sandbox (supervisor performs the authenticated call — Cloudflare's binding model).
- **`Tainted<T, L>`** with `declassify()` as the only integrity-raising operation, `#[track_caller]`,
  logged. Motivated by FIDES, which stopped all tested AgentDojo attacks **and completed 16% more
  tasks than baseline** — structure helps the model.
- **Rule of Two** as a session invariant (untrusted input ∧ confidential access ∧ mutating egress
  ⇒ escalate or refuse).
- **Sandbox backends**: Landlock+seccomp(+user-notif) on Linux 6.12+, userns+seccomp+setrlimit
  below that, Seatbelt on macOS, AppContainer/restricted-token+Job on Windows, `wasmtime`
  (fuel + epoch) for plugin code. **No backend ⇒ hard error**, never silent degradation.
- All backends emit the **same `SandboxReport`**.

**Resolved 2026-08-08 by prototype** — see `crates/frey-core/src/taint.rs` and
`crates/frey-core/tests/taint_ergonomics.rs`. The criterion was *"can a competent Rust developer
write a new tool without ever mentioning a label?"* **Yes**, and the prototype changed the design
in two ways worth recording:

1. **Two independent axes, not four opaque labels.** `Tainted<T, I = Low, C = Public>` where
   integrity ∈ {`High`, `Low`} and confidentiality ∈ {`Public`, `Secret`}. Combining takes the
   *meet* of integrity and the *join* of confidentiality (`zip`), which is the standard IFC
   lattice and needs only 4+4 trait impls instead of 16. The type defaults are the **safe**
   position, so `Tainted<String>` already means untrusted-and-public.
   Runtime `Provenance` carries the story; the type carries only the lattice position. Splitting
   those two concerns is what made the ergonomics work.
2. **Labels live at the boundary, not in tool bodies.** A tool is written
   `fn fs_read(path: &WorkspacePath) -> Result<String, ToolError>` — no label anywhere. The
   framework wraps every return value as `Untrusted<T>`, and the *argument newtype* is what forces
   the check: `WorkspacePath` can only be produced by `Tainted::validate::<InWorkspace>()`, which
   narrows the type and raises integrity in one audited move. Endorsement is therefore the
   *default path*, performed by a parser, with no human in the loop and no `endorse()` call.

Correct IFC vocabulary is used: **`endorse`** raises integrity, **`declassify`** lowers
confidentiality. Both are `#[track_caller]` and write an `AuditEvent` naming the caller's file and
line. `downgrade()` moves to a safer position and is deliberately free and unaudited.

The negative case is a **compile** error, and the diagnostic is legible enough for a coding agent
to fix unaided (`tests/ui/`, trybuild):

```
expected `Tainted<String, High>`, found `Tainted<String, Low>`
```

*Falsifying test:* `tests/taint_ergonomics.rs::a_tool_author_never_writes_a_label` — if that test
ever needs a label in the tool body, the abstraction has failed and the fallback below applies.

*Fallback if it degrades later:* keep `Provenance` and the two audited operations as runtime
constructs and drop the phantom parameters. Still enumerable, still logged, but the negative case
becomes a runtime check.

## ADR-0012 — Secure shell takes `argv`, never a command string — **Accepted**

Parse, don't pattern-match. Allowlist by AST shape (Codex's PowerShell AST analysis is the
precedent), empty environment by default, explicit FS scopes with a COW overlay, deny-all egress
with hostnames resolved once at start (kills DNS rebinding), resource limits, audit record per
invocation, output returned as `Tainted<String, LowIntegrity>` with an explicit elided-byte count.

## ADR-0013 — Code mode engine — **Open (needs a spike)**

Candidates: `rquickjs` (QuickJS: small, fast start, easy host bindings, no network by default),
`deno_core`/V8 (closest to Cloudflare's model, heavy), a WASM component runtime (deterministic,
but requires compiling the scripting language), or delegating to provider-native PTC.

Frey will always delegate when `ProviderCapabilities::programmatic_tool_calling` is true. The
question is only the fallback engine. Decide with a measured spike: cold start, RSS, binary size
impact, and how naturally capability bindings express as host functions.

Note we generate the typed `.d.ts` **even when code mode is off**, because it doubles as the
description corpus for deferred-tool search.

## ADR-0014 — Determinism via an event-sourced run journal; durable engines are adapters — **Accepted**

Every non-deterministic effect (model response, tool result, clock, RNG, MCP list) is appended
with a sequence id. Replay feeds the journal back and fails loudly at the first request-fingerprint
divergence. That single mechanism yields regression tests over real transcripts, `frey replay`,
crash resumption, and cheap prompt A/B on recorded runs — the ecosystem gap nobody has filled.

Temporal (`temporalio-sdk`, whose deterministic `select!`/`join!` wrappers exist precisely because
ordinary tokio combinators break replay) and Restate are **later adapters**, not core dependencies.

## ADR-0015 — The internal event bus **is** the AG-UI model — **Accepted**

AG-UI is the settled agent↔frontend protocol (≈16 event types, ordered JSON event stream over
HTTP, event-sourced state diffs, frontend tool calls, interrupts; adopted by AWS Bedrock
AgentCore and Microsoft Agent Framework). Building the internal bus in its shape means the
harness gets a frontend protocol with no adapter, and backpressure has an obvious rule:
**drop presentation deltas, never semantic events.**

## ADR-0016 — Licence: **MIT OR Apache-2.0** dual — **Accepted**

Rust ecosystem norm for libraries; maximum adoption friendliness with an explicit patent grant
available via the Apache arm. `frey` **is available on crates.io** (verified 2026-08-08: the
registry API returns 404 for the name). Reserve it before the first public announcement.

## ADR-0017 — v1 includes A2A and multi-agent — **Accepted** *(scope decision, 2026-08-08)*

A2A **v1.0** shipped 2026-04-09 under the Linux Foundation (150+ supporting orgs; TSC includes
AWS, Cisco, Google, IBM, Microsoft, Salesforce, SAP, ServiceNow). Frey ships client **and** server,
plus first-class sub-agents. See `architecture/02-protocols.md` and `architecture/05-multi-agent.md`.

The decisive technical reason it belongs in v1 rather than later: **A2A validates ADR-0010.**
Its eight-state task lifecycle contains `TASK_STATE_INPUT_REQUIRED` and `TASK_STATE_AUTH_REQUIRED`
as *interrupted* (non-terminal) states — the same shape as MCP's MRTR `input_required` and AG-UI's
interrupt. Three independent protocols converged on one concept. If we model `NeedsInput` once and
project it three ways, A2A support is mostly free; if we bolt A2A on later, we will have already
built the wrong core type.

## ADR-0018 — Every feature ships with tests; no exceptions — **Accepted** *(2026-08-08)*

Not a process nicety — it is what makes the framework's claims checkable and is the only way a
coding agent can safely modify it (R13). See `architecture/07-testing.md` for the six test tiers
and the rule that **every ADR names the test that would falsify it.**

---

## Verification results (2026-08-08)

| # | Assumption | Result |
|---|---|---|
| 1 | `rmcp` exposes the 2026-07-28 types | **Confirmed.** v3.1.2 (2026-08-07) has `InputRequiredResult`, `ResultType`, `DiscoverResult`, `SubscriptionsListenRequest`, `CacheScope`, and feature-gated `RequestStateCodec` (HMAC-SHA256-sealed `requestState`). No standalone `CacheableResult` struct — hints appear to live on individual list results. **Still to do:** read the source to confirm `ttlMs` placement. |
| 2 | Anthropic's custom tool-search hook scales | **Mechanism confirmed** (return `tool_reference` blocks from an ordinary `tool_result`; every referenced tool must have a definition in `tools`, normally `defer_loading: true`). **Scale unverified** — needs a 500-tool fixture. Note the hard limits: 10,000 deferred tools/request, 5 results/search, 200-char regex, 500-char BM25. |
| 3 | `Tainted<T, L>` ergonomics | **Open.** Prototype `fs_read` / `http_get` / `shell` before it enters the public API. Fallback documented in ADR-0011. |
| 4 | Landlock ABI 6 / Linux 6.12 floor | **Qualified pass.** ABI 6 landed in 6.12 (an LTS kernel). Debian 13 = 6.12; CentOS Stream 10 = 6.12; Ubuntu 26.04 LTS = 7.0. **But RHEL 9.6 is still on 5.14**, and Landlock requires `landlock` in the `lsm=` boot parameter even when compiled in. ⇒ Frey must **detect the ABI level at runtime**, use `rust-landlock`'s best-effort compatibility, and record the achieved level in `SandboxReport`. Never assume 6. |
| 5 | QuickJS is viable for code mode | **Yes, with a caveat.** `rquickjs` 0.12.2 (2026-07-27) exposes `set_memory_limit`, `set_max_stack_size`, `set_interrupt_handler` (periodic callback; return `true` to raise — our fuel/timeout), `set_gc_threshold`, `execute_pending_job`, and feature-gated module loaders. **Caveat:** `Runtime`/`Context` are behind a mutex and the docs discourage use in async contexts. ⇒ one `Runtime` per script execution on a `spawn_blocking` thread, host calls bridged to the async world over channels. That constraint happens to give us exactly the fresh-isolate-per-run model Cloudflare uses. **ADR-0013 resolved to `rquickjs` as the default engine**, with `deno_core`/V8 behind an optional feature. |
| 6 | `frey` available on crates.io | **Yes** — 404 from the registry API. |
| 7 | The wedge is still unoccupied | Re-run the research 03 §2 adversarial pass immediately before the README goes public. |
