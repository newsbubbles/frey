# FAQ

## Should I use this?

Probably not yet, if you need something dependable this quarter. It is 0.x, the API will change,
and nobody runs it in production including its author.

Use it if you want the context window treated as a managed resource, if you are building an MCP
server in Rust, or if you are going to be audited and want capability scoping and information-flow
labels to be structural rather than something you added afterwards.

## Why not Rig, AutoAgents, or OpenFANG?

They are good and you should look at them. Frey exists because of one difference: **the prompt is a
planned artefact rather than a string you concatenate.** Every turn recomputes a cache plan against
the provider's real rules, and the loop refuses breakpoints the plan says are worthless.

Frey is not the first thing to notice that prompt caches break —
[`make-agents-cheaper`](https://github.com/Just-Agent/make-agents-cheaper) and
[LeanCTX](https://github.com/yvgude/lean-ctx) both attack this in Rust. Both sit *beside* an agent.
Frey is the first Rust framework where the cache plan is a core type recomputed every turn.

That claim was deliberately attacked before it was made — see
[`notes/research/03`](../notes/research/03-landscape-and-wedge.md) §2 and §5. The re-check narrowed
the claim and also found a rule missing from Frey's own planner, which is the argument for doing it.

## Why is `Untrusted` a type rather than a lint?

Because a lint is advice and a type is a compiler error. Passing tool output somewhere that needs
trusted input does not compile, and those forbidden flows are `compile_fail` doctests so the
compiler proves them on every platform.

The practical payoff is audit. "Where does untrusted data become trusted?" is answered by
`grep endorse` plus a log with file and line, rather than by reading the whole codebase.

## Does this stop prompt injection?

No. Nothing does, and anyone claiming otherwise is selling something.

What is here reduces blast radius: capability scoping, egress allowlists, information-flow labels,
grants that only ever narrow, and approval prompts showing the literal action rather than a summary
— because a summary is exactly where an injected instruction hides from the person approving it.

## Why does my cheap model produce garbage?

Two failure modes worth separating, both measured during testing:

**Floods of tool calls.** One 8B model emitted ~145 in a single response. Frey caps at 32 per turn
and refuses the excess with an error the model can act on. Tune with `.max_tool_calls_per_turn(n)`.

**Arithmetic.** Three of five models tested fetched every value correctly and then added them up
wrong. That is not a tool-calling failure and no framework fixes it. Give the agent a calculator.

## Why is `cost` sometimes `None`?

Because the provider did not say. OpenRouter reports cost; Anthropic and OpenAI do not. Frey never
invents a number — `None` rather than zero, because a zero in a UI reads as "this was free", which
is a different claim from "nobody said". `run.totals.unmetered_calls` counts how many calls were
silent so a UI can say "plus N unmetered calls".

## What is actually missing?

Honestly, from [the README](../README.md#honest-limitations) and
[`notes/dogfood/`](../notes/dogfood/):

- **Code mode only works by delegation.** There is no embedded JavaScript engine, and you cannot
  supply a small executor instead: [abacus](https://github.com/newsbubbles/abacus) showed that a
  model handed a typed API writes that language, and handed a restricted grammar invents the
  collection helpers it expects to exist. `Strategy::Local` has nothing behind it and will until an
  engine is embedded.
- The `AgentCli` delegation path has never been run against a live vendor binary.
- Landlock's ABI level is not detected by syscall; `doctor` reports the conservative answer.
- `ToolHost::definitions` is sync and infallible, so an adapter from `Toolset` swallows a listing
  failure. Breaking change, intended before 0.2.
- Only OpenRouter, Anthropic, OpenAI and OpenAI-shaped endpoints have adapters.

## Why "Frey"?

A Norse god associated with prosperity and good harvests, which is a joke about token bills.

## Can I use it from Python or TypeScript?

Not directly. Build an MCP server with it and call that from anything — which is most of why
[being a server](mcp.md#being-a-server) exists.

## How do I report something?

[Issues](https://github.com/newsbubbles/frey/issues). Findings that say what was awkward are more
useful than feature requests; the three demo projects each carry a `FINDINGS.md` in that spirit and
all three changed the framework or its documentation.
