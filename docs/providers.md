# Providers

Every adapter splits into a pure `Dialect` — the wire mapping, no I/O — and one shared
`HttpProvider` that owns retry, error classification, timeouts, and stream decoding. That split
means all the provider nuance below is tested without a network, a key, or a mock server, and retry
policy is written once instead of three times.

## Choosing

```rust
HttpProvider::new(Arc::new(OpenRouter), "https://openrouter.ai/api/v1",
                  Auth::Bearer { env: "OPENROUTER_API_KEY".into() })?;

HttpProvider::new(Arc::new(Anthropic), "https://api.anthropic.com",
                  Auth::Header { name: "x-api-key".into(), env: "ANTHROPIC_API_KEY".into() })?;

HttpProvider::new(Arc::new(OpenAiResponses), "https://api.openai.com/v1",
                  Auth::Bearer { env: "OPENAI_API_KEY".into() })?;
```

Credentials are **named, not passed** — read from the environment at request time, so a key never
enters your configuration, a struct, or a log line.

## The differences that cost money

| | Anthropic | OpenAI Responses | OpenRouter |
|---|---|---|---|
| Caching | explicit, ≤4 breakpoints | automatic ≥1024 tokens | automatic, upstream's |
| **Breakpoints Frey places** | **4** | **0** | **0** |
| Min cacheable prefix | 512–4096, **per model** | 1024 | upstream's |
| `input_tokens` | excludes cached | includes cached | normalised to exclude |
| Reports cost | no | no | **yes** |
| Reasoning | — | encrypted, must be replayed | upstream's |

**Read the breakpoint row before planning around the cache planner.** Only one of the three dialects
takes explicit breakpoints. On the other two the provider caches the prefix itself, so there is
nothing for Frey to place and it places nothing — and the planner's *warnings* are what it
contributes there: churn and minimum-prefix, both of which cost exactly as much on an
automatic-caching provider.

That row comes from a measurement, not from this table. `frey doctor` encodes a representative
request through each adapter and counts the `cache_control` markers that come out, and a test
asserts the invariant that an adapter accepting breakpoints must emit them. It found two bugs on its
first run: the Anthropic adapter honoured only the last mark of a four-mark plan, and the Responses
adapter declared an explicit mode the API does not have — so the planner placed a breakpoint on every
OpenAI request and the adapter dropped it, silently, while the plan reported it as placed.

**Anthropic's minimum cacheable prefix varies eightfold between models from one vendor** — 512 on
Opus 5, 4096 on Haiku 4.5. A prompt that caches fine on one silently does not cache at all on the
other, with no error from anyone. Frey's profiles carry this per model.

**Anthropic search only 20 content blocks backward from a breakpoint.** A single agentic turn with
several tool calls can exceed that, making the *next* request miss cache entirely. Frey warns:
`LookbackExceeded { blocks, limit }` — and the check takes a block count and no capabilities, so it
applies Anthropic's 20 to every provider, including ones with no published figure. That is an
overreach in the warning's wording rather than in its arithmetic, and it is recorded as such in
`claims.toml`.

**OpenRouter always reports `cost`,** and is the only supported provider that does. Frey never
invents a number a provider did not give: `run.cost` is `None` rather than zero, because a zero in a
UI reads as "this was free", which is a different claim from "nobody said".

**OpenAI's encrypted reasoning must be replayed verbatim.** Frey asks for it (`store: false` plus
the include) and round-trips it byte for byte. Dropping it is silent, makes answers worse, and costs
money to regenerate.

## Failures that end a run

`402` is **fatal and never retried**. Exhausted credit returns fast and looks transient, so a naive
retry loop turns every remaining turn into a silent no-op while still being billed. `401` and `403`
are the same. `429` is retried, honouring `retry-after`.

OpenRouter also answers **`200` with an error object** when an upstream provider fails or moderates.
Frey surfaces that error's own message rather than reporting "no choices" and discarding the one
sentence that says which.

## Timeouts

Two clocks, because slow and hung are different failures:

```rust
HttpProvider::with_timeouts(dialect, url, auth, Timeouts {
    connect_ms: 10_000,
    read_ms: 300_000,
})?;
```

`read_ms` is the gap *between reads*, not a deadline for the request, so a streaming response resets
it on every chunk and a slow generation never trips it. The default read budget is deliberately
large: a non-streaming request to a slow reasoning model produces no bytes at all until the
generation finishes. One model tested took 98 seconds for three turns.

## A provider Frey has never heard of

Most are OpenAI-shaped. No code required:

```rust
let dialect = OpenAiChat::new("my-gateway");
HttpProvider::new(Arc::new(dialect), "https://gateway.internal/v1", Auth::Bearer { env: "GW_KEY".into() })?;
```

`OpenAiChat` deliberately has no `Default`: an endpoint with an empty provider id would produce
ledger entries and audit records naming nothing. It also never claims to report cost, because a
plain chat endpoint does not.

For something stranger, `ProviderConfig` is serde-round-trippable, so a `frey.toml` and the builder
cannot drift apart; a test pins that. Beyond that, implement `Dialect` — two methods and a
capabilities function, all pure.

## Riding a subscription instead of paying per token

```rust
let agent = AgentCli::claude_code();
let events = agent.delegate(DelegatedTask {
    prompt: "Summarise the changes on this branch".into(),
    workspace: ".".into(),
    allowed_tools: Some(vec!["Read".into(), "Grep".into()]),
    timeout_ms: 120_000,
}).await?;
```

This runs the **vendor's own binary**, and that is a compliance requirement rather than an
implementation convenience: Anthropic's usage policy prohibits third-party applications from using
subscription OAuth credentials. Frey never stores, mints, refreshes or replays a vendor token —
there is nowhere in the API to put one, and a test asserts nothing credential-shaped reaches the
command line.

That constraint is also why `AgentProvider` has no completion method. A delegated agent runs its own
loop, its own tools and its own sandbox; Frey did not mediate those calls and does not pretend
otherwise. `AgentEvent::ToolUsed` is display-only and the audit record says the call was unmediated.

> **Untested end to end.** The wire format is tested against recorded output, but the machine this
> was written on had no working `claude` install. Verify it yourself before relying on it. Only
> Claude Code has an adapter; Codex and the rest are not implemented.

## Testing without a provider

See [Testing an agent](testing.md). `ScriptedModel` records what the model was *shown*, which is how
you assert that your tool block is stable and your prompt is cacheable.
