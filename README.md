# Frey

**A Rust agent framework where the context window is a managed resource.**

[![CI](https://github.com/newsbubbles/frey/actions/workflows/ci.yml/badge.svg)](https://github.com/newsbubbles/frey/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#licence)

Most agent frameworks treat the prompt as a string you concatenate. Frey treats it as what it
actually is: a scarce, cache-sensitive, ordered resource with a budget and a price. Tools, skills,
and code-mode are three presentations of one progressively-disclosed catalog, and nothing degrades
quietly.

> **Status: 0.x, pre-release.** The API will change. It is complete enough to build on and to
> criticise; it has not been run in production by anyone, including its author.

---

## The four things that are actually different

### 1. The cache planner refuses to waste your money

It knows each provider's rules — Anthropic's four breakpoints and per-model minimum prefix,
OpenAI's automatic caching and routing key, OpenRouter's per-upstream split — and it will not place
a breakpoint on a segment that changed last turn.

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
| `frey-tools` | The layers every tool call passes through, and `#[frey::tool]` |
| `frey-agent` | The loop, the journal, replay, multi-agent spawning |
| `frey-mcp` | Model Context Protocol, at the stateless `2026-07-28` revision |
| `frey-sandbox` | Cross-platform confinement that fails closed |
| `frey-a2a` | Agent-to-agent interoperability |
| `frey-harness` | Sessions, approvals, AG-UI, `doctor` |
| `frey-testkit` | A scripted model and hostile fakes, for testing *your* agent |

---

## Built on the current protocols

**MCP `2026-07-28`** removed the stateful core of the protocol — no handshake, no session id, no SSE
resumability — and replaced server-initiated requests with a retry pattern. Frey is built on that
revision, with a shim for older servers, and treats an MCP server as the untrusted party it is:
listings are re-sorted defensively so a server cannot churn your prompt cache, freshness hints are
capped, and catalogs are private unless the server says otherwise.

**A2A v1.0** and **AG-UI** are first-class, because all three protocols converged on the same
concept: a task that is alive and waiting for something from outside it. Frey models that once and
projects it three ways — which is also why a human approval, an MCP elicitation, an A2A auth
challenge, and a frontend-executed tool are one code path.

---

## Honest limitations

- **Nobody has run this in production.** The design is researched and the tests are real; the
  operating experience is not there yet.
- **Code mode is partial.** The typed API generator, the capability bindings, and delegation to a
  provider that can run the script all work. An embedded JavaScript engine is not in the default
  build — for Anthropic the correct implementation is delegation, and pulling a JS runtime into
  every build of a Rust framework is a cost most users would pay for nothing.
- **The sandbox reports what it can enforce, and it is often less than you want.** Landlock needs a
  6.12 kernel *and* `landlock` in the `lsm=` boot parameter; RHEL 9 has neither. Frey tells you
  precisely what is missing and refuses to run rather than pretending.
- **`defer_loading` saves context, not bandwidth.** Anthropic still require every tool definition on
  every request.
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

Regenerate the compile-fail expectations after a toolchain upgrade:

```bash
TRYBUILD=overwrite cargo test -p frey-core --test ui
```

The design record lives in [`notes/`](notes/README.md): the research it is based on, the
architecture, eighteen ADRs with the reasoning behind each decision, and
[`notes/PROGRESS.md`](notes/PROGRESS.md), which records what each milestone found — including the
bugs, the scope reductions, and the two decisions that turned out to be wrong.

## Licence

MIT OR Apache-2.0, at your option.
