# Dogfooding, day 1 — live providers

Running log. Appended as things are found, not tidied afterwards. The point of this file is that a
future reader can reproduce a problem, not that it reads well.

Everything here comes from real traffic against OpenRouter, paid for with a real key, using
`cargo run -p frey --example live_openrouter -- <model>`.

## The headline

**The framework works end to end on the first live run.** No changes were needed to make a real
agent, against a real provider, with real tool calls, produce a correct answer:

```
== qwen/qwen3-30b-a3b-instruct-2507 ==
  answer:   1101
  correct:  true  (expected 1101)
  elapsed:  3.4s
  journal:  7 effects: model-response → tool-result → model-response → tool-result → tool-result → tool-result → model-response
  usage:    3 calls, 463 in, 87 out, 864 cached
  complete: true
  cost:     CostEstimate { amount: Money { micros: 159, currency: Usd }, source: Reported }
  warnings: none
```

That is the whole loop: request serialisation, tool block, tool-call parsing, tool dispatch through
the layers, results fed back, usage accounting, cost from the provider rather than estimated, and a
journal. 3.4 seconds and $0.000159.

The rest of this file is what went wrong.

---

## F1 — There is no cap on tool calls *per turn*. Severity: high.

`meta-llama/llama-3.1-8b-instruct` emitted roughly **145 tool calls in a single response**. Frey
executed every one of them. The run recorded **267 journal effects** across 4 model calls, burned
23,259 input and 7,110 output tokens, and cost **10× the successful run** — for a wrong answer.

The loop bounds *turns*:

```rust
for turn_index in 0..self.max_turns {      // crates/frey-agent/src/run.rs:166
    ...
    for call in calls {                    // :267 — unbounded
```

`max_turns` is not the protection it looks like. One turn can contain unlimited work.

Why this matters more than the token bill: in this example the tools are pure functions over a
constant. The same runaway against a tool that sends email, writes a file, or bills per call is not
a cost incident. Nothing in the framework currently refuses.

This is squarely against the project's own rule that nothing degrades quietly. It did not degrade
quietly in *cost*, but it degraded silently in *behaviour* — no warning names the runaway, no cap
stops it, and the operator finds out from the invoice.

**Fix wanted:** a `max_tool_calls_per_turn` on `Agent` with a sane default, and a `Warning` when a
response is truncated against it. The truncation must be visible to the model too, or it will simply
re-issue the dropped calls next turn.

Reproduce: `cargo run -p frey --example live_openrouter -- meta-llama/llama-3.1-8b-instruct`

## F2 — No HTTP timeout anywhere. Severity: high.

`HttpProvider::new` builds the client with only a user agent:

```rust
let client = reqwest::Client::builder()
    .user_agent(concat!("frey/", env!("CARGO_PKG_VERSION")))
    .build()
```

`reqwest`'s default is **no timeout at all**. A provider that accepts a connection and never
responds hangs the agent forever, with no error, no warning, and nothing in the journal.

This is not hypothetical: `z-ai/glm-4.7-flash` legitimately took **97.7 seconds** for a three-turn
run in this session. Slow-but-alive and hung-forever currently look identical from the outside, and
neither is bounded.

There *is* a `timeout_ms` field in the codebase — on `DelegatedTask`, at
`crates/frey-core/src/provider.rs:313`. It is read nowhere (see F3). So the only timeout-shaped
thing in the framework is dead code, which is worse than none: it reads as though the concern was
handled.

**Fix wanted:** a connect timeout and a per-request timeout on the client, both configurable, both
defaulted to something finite. A streaming request needs a *read* timeout rather than a total one,
since a long generation is not a hang.

## F3 — `AgentProvider` has no concrete implementation. Severity: high (it is a headline feature).

R4 of the founding brief asks for Claude-SDK / Codex-SDK style adapters so users can ride an
existing subscription instead of paying per token. What ships in v0.1.1 is the *trait*, plus a stub
named `Stub` inside a `#[cfg(test)]` module:

```
crates/frey-core/src/provider.rs:381:  pub trait AgentProvider: Send + Sync {
crates/frey-core/src/provider.rs:521:      impl AgentProvider for Stub {     // in tests
```

There is no adapter that runs `claude`, `codex`, or anything else. The `DelegatedTask` type — with
its `workspace`, `allowed_tools` and `timeout_ms` fields — is a description of work nothing
performs.

This is not recorded in the README's limitations list, which mentions code mode, Landlock, live
tests and production experience, but not this. **The README currently implies a capability the
crate does not have.** That is the most important thing found today, because it is the one a new
user would hit within an hour of arriving from the repo.

## F4 — Two of the three requested demo projects need an MCP *server*, which does not exist.

`frey-mcp` is client-only: `client.rs` and `protocol.rs`, no server module.

```
crates/frey-mcp/src/
  client.rs
  protocol.rs
  lib.rs
```

Frey can *consume* MCP servers and cannot *be* one. For a framework whose first listed property is
"MCP-native", and in a year where the interesting half of the spec is what a stateless server looks
like, that is a real gap rather than a missing convenience.

Being fair to the original scope: nothing in the seed requirements literally asked Frey to serve
MCP; R1 says "MCP-native tooling built on the latest spec including stateless servers", which was
read as *supporting* stateless servers as a client. Re-reading it now, "including stateless servers"
most naturally means Frey should be able to build one.

## F5 — `LookbackExceeded` fired correctly, under conditions no unit test produced. Positive.

The 20-block lookback check — added late, only because the adversarial re-check went looking for a
reason the novelty claim was wrong — was the *only* diagnostic that fired during the F1 runaway:

```
warning:  LookbackExceeded { blocks: 298, limit: 20 }
warning:  LookbackExceeded { blocks: 202, limit: 20 }
warning:  LookbackExceeded { blocks: 26, limit: 20 }
```

It is diagnosing a cache problem rather than a runaway, so it is not a substitute for F1's fix. But
it is the feature earning its place on its first contact with a real model, and it reported a
condition that the scripted-model tests had never produced.

## F6 — Cost and usage accounting is correct and complete on live traffic. Positive.

Every run reported `source: Reported` rather than an estimate, `is_complete()` was true throughout,
and cached tokens were counted separately from full-price ones. The normalisation is deliberate and
tested (`cached_tokens_are_subtracted_from_the_prompt_total`): `input` means *tokens billed at full
rate*, which is why a run can show more cached tokens than input tokens and still be consistent.

One arithmetic question is still open, noted here so it is not lost:
`total_input() = input + cache_read + cache_write`, which for the unit test's fixture
(`prompt_tokens: 2000, cached: 1500, cache_write: 500`) yields **2500 against a reported 2000**. If
OpenRouter's `prompt_tokens` already includes cache-write tokens, `total_input()` overcounts by the
write volume on any turn that writes to cache. Not yet confirmed against live data — no model tested
today reported a non-zero `cache_write_tokens`.

---

## Model-by-model, same task, same tools

The task: discover three station names, fetch a reading from each, report the sum (1101).

| Model | Correct | Time | Model calls | Cost | Note |
|---|---|---|---|---|---|
| `qwen/qwen3-30b-a3b-instruct-2507` | ✅ | 3.4s | 3 | $0.000159 | parallel fetches, clean |
| `z-ai/glm-4.7-flash` | ✅ | 97.7s | 3 | $0.000168 | correct but 29× slower |
| `openai/gpt-oss-120b` | ❌ 1001 | 8.8s | 5 | $0.000098 | serial fetches; arithmetic wrong |
| `mistralai/mistral-nemo` | ❌ 1091 | 6.5s | 3 | $0.000059 | correct tool use; arithmetic wrong |
| `meta-llama/llama-3.1-8b-instruct` | ❌ | 9.7s | 4 | $0.001733 | runaway, see F1 |

Two observations worth carrying into the tool-presentation work:

- **The failures are arithmetic, not tool use.** Three of five models fetched all three readings
  correctly and then added them up wrong. That is a strong argument for the framework making it easy
  to give the model a calculator, and a caution against benchmarking "tool calling" with a task that
  secretly also tests mental arithmetic.
- **Parallel vs serial tool calling varies by model at identical prompt.** `qwen` issued three
  `fetch_reading` calls in one response; `gpt-oss-120b` issued them one per turn, costing two extra
  round trips. Any per-turn cap from F1 must not punish the parallel model, which is the one
  behaving well.
