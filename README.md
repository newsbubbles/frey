# Frey

**A Rust agent framework where the context window is a managed resource.**

[![CI](https://github.com/newsbubbles/frey/actions/workflows/ci.yml/badge.svg)](https://github.com/newsbubbles/frey/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#licence)

Most agent frameworks treat the prompt as a string you concatenate. Frey treats it as what it
actually is: a scarce, cache-sensitive, ordered resource with a budget and a price. Every turn
recomputes a cache plan against the provider's real rules, and nothing degrades quietly.

The larger design — tools, skills and code-mode as three presentations of one progressively-disclosed
catalog — is built and **not yet wired into the agent loop**, which takes a fixed tool list once per
run. The pieces are real and tested; calling them is currently yours to do. Recorded in the
[capability audit](notes/audit/01-capability-audit.md).

> **Status: 0.x, pre-release.** The API will change. It is complete enough to build on and to
> criticise; it has not been run in production by anyone, including its author.
>
> Every claim on this page has a row in **[`claims.toml`](claims.toml)** with a status and a link to
> whatever stands behind it, checked on every push. Today that is **60 rows: 29 settled by a named
> test, 1 operated, 5 tested-only, 15 unevidenced, and 10 retracted** — retracted meaning the claim
> was made here and is now withdrawn, kept in the file because deleting it would hide that it was
> ever made.
>
> The one `operated` row is the cheapest kind available: a dated record of what ten third-party MCP
> servers do, which costs nothing to run and expires in 120 days. It says nothing about Frey — an
> earlier version of this paragraph claimed it did, which is [I-011](notes/INCIDENTS.md).
> **Nobody has run Frey unattended, including its author** — `operating.unattended` is unevidenced
> and the thirty-night record that would settle it has not started. 506 tests pass; a passing test
> is not an operating hour and the file does not let the two be confused.
>
> That split is the point of the file rather than an admission inside it. A README is a snapshot and
> code is not: two claims here were flatly *wrong* for as long as they had been written before an
> audit found them, and [`notes/INCIDENTS.md`](notes/INCIDENTS.md) records ten more found since,
> each with a `found_by` field saying whether an instrument caught it or a person did.

## Built with it

Three demonstration projects, written to find out what using Frey is actually like rather than to
advertise it. Between them they found **two real bugs** — both in the MCP server, both fixed the
same day — a handful of rough edges that are not fixed, and one result that changed what this README
says about code mode. Each carries a `FINDINGS.md`, including the parts that are still awkward.

| Project | What it is |
|---|---|
| **[thicket](https://github.com/newsbubbles/thicket)** | Graph-shaped agent memory, served over MCP. One `Toolset`, exposed both as an MCP server and to an in-process agent. |
| **[switchboard](https://github.com/newsbubbles/switchboard)** | A hosted, stateless MCP server on HTTP, with approval gates. Round-robins an approval handshake across two replicas to prove statelessness. |
| **[abacus](https://github.com/newsbubbles/abacus)** | Measures tool calling against code mode, and finds that models will not write a restricted mini-language. |

Start with thicket for tools and the agent loop, switchboard for the protocol, abacus for the
uncomfortable result.

---

## The four things that are actually different

### 1. The cache planner refuses to waste your money

It knows each provider's rules — Anthropic's four breakpoints and per-model minimum prefix,
OpenAI's automatic caching and routing key, OpenRouter's per-upstream split — and it will not place
a breakpoint on a segment that changed last turn.

**Breakpoints reach one dialect by default.** OpenAI Responses and OpenRouter cache automatically,
so there is nothing to place and Frey places nothing; what it contributes there is the warnings,
which cost exactly as much to ignore. `OpenRouter::with_explicit_cache()` opts into placing them for
`anthropic/*` upstreams, which is the only family whose passthrough is documented.

`frey doctor` prints the split from a **measurement** rather than a table: each adapter encodes a
real request and the markers in the result are counted. That check found two bugs on its first run,
and it is the reason the row for OpenRouter reads three of four rather than four — a Chat Completions
tool object has no content part to carry a marker, so the tool-block breakpoint is refused there
rather than emitted somewhere the upstream will not read it. See
[the caching model](docs/context-and-caching.md#which-of-this-reaches-which-provider).

```
$ cargo run -p frey --example cache_planning

== a stable system prompt ==
  cached prefix: 12800 tokens
  warnings: 0

== the same prompt, with a clock in it ==
  cached prefix: 12000 tokens
  churn in `system`: 800 tokens rewritten every turn
    the system prompt changed between turns. A clock, a session id, or a token
    count interpolated into it will do this.
```

No provider reports that second case. The only symptom is the bill.

It also catches the subtler one: Anthropic search 20 content blocks backward from a breakpoint, and
a single agentic turn with several tool calls can exceed that — making the *next* request miss cache
entirely, again with no error from anyone.

Frey is not the first thing to notice that prompt caches break; [`make-agents-cheaper`][mac] and
[LeanCTX][lean] both attack this in Rust. It is the first framework where the cache plan is a core
type, recomputed every turn from provider capabilities, and where the loop refuses a breakpoint the
plan says is worthless.

[mac]: https://github.com/Just-Agent/make-agents-cheaper
[lean]: https://github.com/yvgude/lean-ctx

The same example shows a prompt that caches fine on Opus 5 and **silently does not cache at all** on
Haiku 4.5, because the minimum cacheable prefix differs eightfold between two models from one vendor.

### 2. Untrusted data is a type

Everything from outside — a tool result, a fetched page, an MCP server's own description, a peer
agent's reply — is `Tainted`. Passing it somewhere that needs trusted input does not compile:

```
expected `Tainted<String, High>`, found `Tainted<String, Low>`
```

Those forbidden flows are `compile_fail` doctests, so the compiler proves them on every platform
rather than a snapshot of diagnostic text pinning them to one.

Raising integrity happens in one auditable place, records its call site, and is usually done by a
parser rather than a human — narrowing a type *is* the check. An auditor asking "where does
untrusted data become trusted?" gets `grep endorse` plus a log with file and line, rather than a
reading of the whole codebase.

### 3. Errors are typed by audience

What the model is told, what the operator is told, and what a user sees are three different fields.
A tool failure carries instructions the model can act on:

```rust
Err(tool_err!(NotFound, "no file at {path}")
    .guide("List the directory with `fs_list` before reading.")
    .suggest(["fs_list"]))
```

A test asserts that operator diagnostics can never reach the context window, and another that the
AG-UI projection never sends a stack trace to a browser.

### 4. The journal is the session

Every non-deterministic effect is recorded. Replay reproduces a run exactly and **diverges loudly at
the first mismatch** rather than quietly adapting — a replay that improvises produces confident
results about a run that never happened.

---

## Quick look

```rust
use frey::prelude::*;

let agent = Agent::new(provider, tools, "anthropic:claude-opus-5")
    .system("You are a careful assistant.")
    .max_turns(24);

let run = agent.run("What changed in the release notes this week?").await?;

println!("{}", run.text());
println!("{} calls, cost {:?}", run.totals.by_model.len(), run.cost);
for warning in &run.warnings {
    println!("{warning:?}");   // cache churn, budget pressure, degraded capabilities
}
```

Two runnable examples, neither needing an API key:

```bash
cargo run -p frey --example cache_planning
```

```bash
cargo run -p frey --example agent_loop
```

And a diagnostic for the host you are on:

```bash
cargo run -p frey-cli -- doctor
```

---

## What is in the box

| Crate | What it does |
|---|---|
| `frey` | Facade and curated prelude |
| `frey-core` | Types and traits. No I/O, so the planners are pure functions |
| `frey-context` | Budget, cache planning, discovery, skills, code-mode codegen |
| `frey-providers` | Anthropic, OpenAI Responses, OpenRouter, config-defined dialects |
| `frey-tools` | The layers a tool call passes through, validators to build one from, `#[frey::tool]`. **No tools** |
| `frey-agent` | The loop, the journal, replay, multi-agent spawning |
| `frey-mcp` | Model Context Protocol, at the stateless `2026-07-28` revision |
| `frey-sandbox` | A sandbox *policy* and availability reporting. **No enforcement backend yet** |
| `frey-a2a` | Agent-to-agent interoperability |
| `frey-harness` | Sessions, approvals, AG-UI, `doctor` |
| `frey-testkit` | A scripted model and hostile fakes, for testing *your* agent |

---

## Built on the current protocols

**MCP `2026-07-28`** removed the stateful core of the protocol — no handshake, no session id, no SSE
resumability — and replaced server-initiated requests with a retry pattern. Frey is built on that
revision — **in the server direction only; see the limitation below** — and treats an MCP server as
the untrusted party it is:
listings are re-sorted defensively so a server cannot churn your prompt cache, freshness hints are
capped, and catalogs are private unless the server says otherwise.

**A2A v1.0** and **AG-UI** share MCP's shape, because all three protocols converged on the same
concept: a task that is alive and waiting for something from outside it. Frey models that once and
projects it three ways, so a human approval, an MCP elicitation, an A2A auth challenge and a
frontend-executed tool are one type.

Being precise about what that buys you today: the *projection* is real and tested, and MCP is the
only one of the three with a working transport on both sides. `frey-a2a` is types and a lifecycle
with no client or server, and `frey-harness::agui` is a serialiser nothing streams through. The
convergence was worth designing for — it made the MCP server's approval handshake a dozen lines —
but "first-class" was overstating two of the three.

---

## Honest limitations

- **Nobody has run this in production.** Three demo projects have been built on it and it has been
  driven against live models, which is more than nothing and much less than operating experience.
  What that would take, what it would cost, and what would count as evidence is written down in
  [the evidence plan](notes/plan/EVIDENCE-PLAN.md), which is also honest that a careful reader will
  file all of it as sophisticated dogfooding until something that is not this author's own project
  depends on it.
- **The agent-CLI adapter has not been run against a live vendor binary.** `AgentCli` delegates to
  Claude Code so you can ride a subscription instead of paying per token, and its wire format is
  tested against recorded output — but the machine it was written on had no working `claude`
  install, so the end-to-end path is unverified. Treat it as untested until you have tested it.
- **Only Claude Code has a delegation adapter.** The `AgentProvider` trait is general; Codex and
  the rest are not implemented.
- **Code mode requires delegation.** The typed API generator, the capability bindings, and
  delegation to a provider that can run the script all work. There is no embedded JavaScript engine,
  and [abacus](https://github.com/newsbubbles/abacus) established that this is more limiting than
  originally stated: a model handed a typed API writes *that language*, and handed a bespoke
  restricted grammar it invents the `filter` and `first` such a language ought to have. So you
  cannot substitute a small executor of your own. `Strategy::Local` therefore has nothing behind it
  and will keep having nothing until an engine is embedded — delegation is the only working path.
- **There are no tools in the box, and nothing is confined.** `frey-tools` ships the layers a call
  passes through and the validators to build a tool safely — `InWorkspace`, `AllowedProgram`,
  `OnEgressAllowlist` — but no tool, and nothing in Frey spawns a process. `frey-sandbox` is a
  policy language with no enforcement backend, which is consistent since there is nothing to confine.
  R5 asked for "an actually secure shell tool" and that is not done.
- **Progressive disclosure is not in the agent loop.** Tool search, skills and code-mode all exist,
  are tested, and nothing in `Agent::run` calls them — it takes a fixed tool list once per run. The
  catalog machinery is real; the loop does not use it. See the
  [capability audit](notes/audit/01-capability-audit.md).
- **Human approval works over MCP, not in the loop.** A tool returning `NeedsInput` inside
  `Agent::run` is rendered to the model as "approval was not available" and the run continues.
- **The sandbox reports what it can enforce, and it is often less than you want.** Landlock needs a
  6.12 kernel *and* `landlock` in the `lsm=` boot parameter; RHEL 9 has neither. Frey tells you
  precisely what is missing and refuses to run rather than pretending.
- **`defer_loading` saves context, not bandwidth.** Anthropic still require every tool definition on
  every request.
- **Replay compares the prompt, and could not until recently.** `RequestFingerprint` was model, turn
  count and tool names — pure shape — so a journal replayed *green* after the system prompt changed.
  It now carries a content hash. A journal recorded before that change replays for shape only and
  reports so, rather than reporting a match it has not established.
- **The token estimate is `len / 4`.** Every turn now compares it against the count the provider
  returned and warns past 25% error, so the number the budgeter evicts on is at least observable.
  What does not exist yet is the distribution over enough real calls to say where it is bad.
- **Cost figures are estimates** everywhere except OpenRouter, which is the only supported provider
  that reports what a call cost. Frey never invents a number the provider did not give.
- **Prompt injection is not solved**, by Frey or anyone. What is here reduces blast radius:
  capability scoping, egress allowlists, information-flow labels, and approval prompts that show
  the literal action rather than a summary — because a summary is exactly where an injected
  instruction hides from the person approving it.

---

## Development

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

**[Documentation is in `docs/`](docs/README.md)** — getting started, providers, tools, MCP, the
caching model, the security model, and how to test an agent without spending money.

The design record lives in [`notes/`](notes/README.md): the research it is based on, the
architecture, eighteen ADRs with the reasoning behind each decision, and
[`notes/PROGRESS.md`](notes/PROGRESS.md), which records what each milestone found — including the
bugs, the scope reductions, and the two decisions that turned out to be wrong.

## Licence

MIT OR Apache-2.0, at your option.
