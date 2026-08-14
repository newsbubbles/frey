# Why Frey

There are a lot of agent frameworks. Most of them are fine. This page is the argument for using
this one, written so that you can check it — and the argument for *not* using it, in the same
amount of detail, because the second half is what makes the first half worth reading.

## The one-sentence version

**Every agent framework can place a prompt-cache breakpoint. Frey is the one that tells you when
the breakpoint stopped working.**

That distinction sounds small. It is the difference between a 7% cache hit rate and an 84% one, and
the reason it persists is that **no provider returns an error when a cache marker is wasted.** Not
Anthropic, not OpenAI, not OpenRouter. The request succeeds. The answer is correct. You are simply
billed as though the cache were not there — plus a premium, because writing to a cache costs 1.25×
the base rate.

## The failure this exists for

In 2026 ProjectDiscovery ran their security agent with prompt caching enabled and a **7% actual hit
rate, for months**. It was not caught by watching the bill, because the bill looked like an agent
that was getting busier. It was caught by reading `cache_read_input_tokens` off the responses. The
cause was mutable working memory sitting inside a prefix that was supposed to be static, so every
step invalidated everything behind it. Moving that state out took the hit rate to 84% and the cost
down 59%.

Nothing in their stack was broken. Caching was on. The dashboard said so.

There are three ways to lose a prompt cache and **none of them produces an error from anybody**:

| | What happens | What you see |
|---|---|---|
| **Churn** | Something inside the "stable" prefix moved — a timestamp in a tool description, a reordered listing, mutable state | Nothing. The request succeeds. |
| **Below minimum prefix** | The model needs 1,024–4,096 tokens before it will cache at all, varying **eightfold between two models from the same vendor** | Nothing. The marker is accepted and ignored, and you pay the 1.25× write premium for it. |
| **Lookback overrun** | Anthropic searches only ~20 content blocks back from a breakpoint. One agentic turn with several tool calls and results exceeds that | Nothing. The *next* request misses entirely. |

Frey emits `Warning::CacheChurn`, `Warning::BelowMinPrefix`, and `Warning::LookbackExceeded` for
these, every turn, with what each one costs. That is the product.

## What everyone else does, and what they don't

Researched 2026-08-14. The `places` column is genuinely well served — this table is not an argument
that other frameworks are bad at caching. It is an argument that the whole field is solving
*placement* and nobody is solving *diagnosis*.

| Framework | Places breakpoints | Detects churn | Checks min prefix | Checks lookback | Reports on auto-caching providers |
|---|---|---|---|---|---|
| **pydantic-ai** V2 (Python) | **Yes** — `CachePoint`, auto-trims to Anthropic's 4 | No | No | No | Anthropic only |
| **Vercel AI SDK** (TS) | **Yes** — `caching: 'auto'`, picks the strategy per provider | No | No | No | Routes; does not diagnose |
| **LangChain / LangGraph** (Py/JS) | Middleware; v1.0 dropped structured system messages and broke it | No | No | No | No |
| **OpenAI Agents SDK** | Open feature request | No | No | No | — |
| **Microsoft Agent Framework** | Open feature request | No | No | No | — |
| **Rig** (Rust) | No caching layer | No | No | No | No |
| **Frey** | Yes | **Yes** | **Yes** | **Yes** | **Yes** |

pydantic-ai is the strongest of these and worth naming precisely: it has `CachePoint`, per-block
caching, automatic enforcement of the four-breakpoint limit by trimming older points, and
`cache_hit_ratio` in its usage object. That is a real implementation and better plumbing than most.
It reports the ratio *after the fact* and does not tell you *which segment* broke, or that your
prefix was too short to cache in the first place.

**The last column is the one people miss.** On OpenAI and OpenRouter there is nothing to place — the
provider caches the prefix itself — so every framework's cache feature is a no-op there. But the
*waste* is identical: a prefix that churns costs exactly as much on an automatic provider as on an
explicit one. Frey places nothing there and still reports it.

That column was also a lie in Frey until 14 August 2026. Churn detection sat behind an early return
that fired whenever the breakpoint budget was zero — which is *by definition* every automatic
provider, including the only one in production use. Recorded as
[I-001](../notes/INCIDENTS.md), fixed, and pinned by
`churn_is_reported_on_a_provider_that_caches_automatically`.

### Adjacent work, credited rather than competed with

- **`make-agents-cheaper`** and **LeanCTX** — Rust tools that fingerprint prompt layers and relocate
  volatile fields out of the cacheable prefix. Closest prior art. They sit *beside* an agent.
- **TokenPilot** ([arXiv:2606.17016](https://arxiv.org/abs/2606.17016)) — a research prototype doing
  ingestion-aware compaction and lifecycle-aware eviction. Compaction and eviction, not diagnosis.
- **MegaBrain Gateway** and similar — gateways that maintain cache affinity across providers, plus a
  manual `cache_audit.py`.

All of them are worth using and none of them owns the agent loop, which is the thing that makes the
difference: **Frey can refuse to place a breakpoint the plan says is worthless, mid-run, before the
request goes out.** A tool sitting beside the agent can only tell you afterwards.

## The second reason: the claims file

Frey ships [`claims.toml`](../claims.toml) — every claim in its README and docs, each with a status
and a link to whatever stands behind it, **checked in CI on every push**. A test that gets renamed
unsettles the claim that cited it. Evidence with a date on it expires.

Today: **59 rows — 29 settled by a named test, 1 operated, 4 tested-only, 15 unevidenced, 10
retracted.** The retracted ones are claims this repository made and withdrew; they are kept in the
file because deleting them would hide that they were ever made.

**I have not found another agent framework, in any language, that ships one** — stated as a search
result rather than a fact about the world, since it is exactly the kind of claim this file exists to
discipline. The underlying idea is not novel: treating written claims as executable assertions with
evidence adapters has been done for the quantitative claims in research papers. Applying it to a
framework's own README appears to be unusual, and if you know of prior art here, the correct
response is a pull request adding a row to `claims.toml` retracting this paragraph.

If you are the person who has to justify a dependency to a staff engineer or an auditor, that file
is the argument, and it is one you can verify in about ninety seconds without trusting a word on
this page.

Alongside it, [`notes/INCIDENTS.md`](../notes/INCIDENTS.md) has one entry per failure with a
`found_by` field — `system`, `operator`, or `code-reading`. The ratio is the point: a project with
no incidents is indistinguishable from a project with no instruments.

## Feature list, split by what stands behind it

### Settled — a named test establishes it

**Context and cost**
- Cache churn detected and priced, on explicit *and* automatic providers
- Prefix below the model's minimum reported rather than silently uncached
- Up to four breakpoints on Anthropic including the tool block, from a per-model table
- Breakpoints verified to reach the wire by *measuring* each dialect's output, not by a table
- Token estimates reconciled against the provider's own count every turn, warned past 25%
- Cost is never invented: no figure the provider did not give
- The cost on the event stream is the same figure the return value carries

**Failing loudly**
- A tool catalog that cannot be listed ends the run instead of becoming an empty tool block
- An agent given no tools says so
- A run that dies mid-flight carries its journal out with the error
- A run that ends on a provider failure or turn limit still closes its event stream
- A router substituting the model mid-run is reported
- A response asking for more tool calls than permitted has the excess refused individually
- Every warning a caller can read is also one an event-stream watcher can see
- A public variant Frey is supposed to emit and never does is caught in CI by a producer lint

**MCP — read the retracted list below before planning around the client**
- Frey can *be* an MCP server, not only call one, from the same `Toolset` — this half is solid
- Approval that survives a stateless round trip
- Speaks `2026-07-28`: no handshake, no session id, `server/discover`
- A server reordering its listing cannot churn your cache; a server adding a tool is reported

**Type-level integrity**
- Passing untrusted data where trusted data is required **fails to compile**
- Raising integrity happens in one auditable, greppable place
- Combining trusted and untrusted values does not read as trusted

**Knowing what it costs you**
- Every turn reports where its wall-clock went, split into Frey's phases and the two that are not
  Frey's — the provider wait and your tools. `frey timings <journal>` reads it back across a run.
  **~282 µs of framework overhead per turn** on a release build, *on the smallest possible prompt* —
  see [performance](performance.md) for why that caveat is the important half.

**Operating**
- Replay reproduces a run and diverges loudly at the first mismatch
- Replay is reachable from the ordinary loop as a `ModelProvider`
- No vendor subscription token is ever stored, minted, or replayed

### Tested but not run against a live upstream
Lookback checking · OpenRouter breakpoints for `anthropic/*` · agent-CLI delegation

### Declared and **not** evidenced — read this before you depend on it
- **Streaming inside the agent loop.** The adapters decode SSE correctly and that is tested. The
  loop does not stream.
- **Progressive disclosure.** Tools, skills and code-mode as three views of one catalog is built and
  **not wired into the loop**, which takes a fixed tool list once per run.
- The planner beating a competent hand-placed breakpoint — see the honest ceiling below
- **Concurrency at any scale.** `complete(&self)`, `Arc<P>: ModelProvider` and
  `HttpProvider::with_client` exist so a fleet shares one connection pool. No load test does.
- **Overhead on a realistic prompt.** The 282 µs above is one message and no tools; segmentation and
  assembly both scale with prompt size and neither has been measured at scale.
- Human approval inside `Agent::run` · media items · unattended operation

### Retracted — do not use Frey for these
- **The MCP client cannot connect to a real server today.** `McpClient` ships no `Transport` outside
  `#[cfg(test)]`, and the advertised shim for pre-stateless servers does not exist — `negotiate()`
  writes `stateless: false` and nothing reads it, so `initialize` is never sent. Since 0 of 6
  reachable third-party servers speak the stateless revision, that is every real server tested. The
  **server** direction is unaffected. [I-011](../notes/INCIDENTS.md).
- **No spend cap.** There is no `max_cost` anywhere in thirteen crates. Use a provider-side ceiling.
- **Sandbox confinement** — the policy layer is pure and tested; the confinement claim is withdrawn
- **A2A** · **local code-mode execution** (models will not write a restricted mini-language;
  measured in [abacus](https://github.com/newsbubbles/abacus))

## Who this is for

**Use Frey if** you are running many short sessions sharing one large stable prefix, on Anthropic or
through a router, where cache economics are a real line item — and you are in Rust, or you want a
single binary with no Python runtime. Or if you need to hand someone an auditable account of what
your agent framework does and does not do.

**Do not use Frey if** you want breadth — Rig has 20+ providers, 10+ vector stores, WASM, full OTel
and production users at Neon and St. Jude, and it is the right default for most Rust LLM work. Or if
you are productive in Python: pydantic-ai V2 is genuinely strong and its toolset composition is
still the state of the art in tool plumbing. Or if you need streaming in the loop, a spend cap, or
enforced sandboxing today.

## The honest ceiling

`cache.saves-money` — *"the planner beats what a competent developer would place by hand"* — is
**unevidenced**, and it is deliberately the claim this page does not make. Frey can prove it
*detects* the three silent failures. It has not yet proved it *saves more than a careful person
who already knows about them*. The pre-registered A/B that would settle it is in
[`notes/plan/STATUS.md`](../notes/plan/STATUS.md), with a control arm of one hand-placed breakpoint
rather than zero, because three independent readers called the zero arm a strawman.

And `operating.unattended` is unevidenced too. 506 tests pass. **Nobody has run Frey unattended,
including its author.** A passing test is not an operating hour, and the whole apparatus above
exists so those two things cannot be quietly confused.
