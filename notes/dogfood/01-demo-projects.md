# Dogfooding, day 1 — building on it

[`00-live-provider-findings.md`](00-live-provider-findings.md) covers driving Frey against live
models. This covers building two real projects on it, which found different things: the live tests
found *behaviour* bugs, and the projects found *shape* problems.

- **[thicket](https://github.com/newsbubbles/thicket)** — graph-shaped agent memory over MCP.
- **[switchboard](https://github.com/newsbubbles/switchboard)** — a hosted, stateless MCP server.
- **[abacus](https://github.com/newsbubbles/abacus)** — tool calling measured against code mode.

Each repo carries its own `FINDINGS.md` written from the user's side. This file is the framework's
side: what to do about them.

---

## Bugs found by the demos

### D1 — The MCP server could ask for input and never receive the answer. Fixed.

Found by switchboard, immediately, because `deploy` is the first tool anybody writes for a hosted
server and the first thing you want it to do is refuse to act unsupervised.

`Server` emitted `input_required` correctly and there was nowhere for the retry's answers to land:
`ToolCx` had no field for them. Any tool requiring approval was permanently stuck at the asking
stage. Half a handshake.

Fixed with `ToolCx::resume` plus `ToolCx::new`/`resuming` so the added field did not break every
construction site.

Worth recording is the alternative that was rejected. Smuggling the answers into `invocation.args`
under a reserved key would have avoided touching a core type — and Frey's own argument validation
kills it, because a schema with `additionalProperties: false` rejects the injected key. The type
system pointing out that answers are context and not arguments.

### D2 — The MCP server did not validate arguments at all. Fixed.

Found by switchboard sending `"value": "true"` — the string — to a boolean parameter. Nothing
complained; `as_bool()` returned `None`, an `unwrap_or(false)` fallback took over, and the tool
**disabled a flag the caller meant to enable**. A production change, silently inverted, reported as
a success.

The cause was a layering accident. `check_arguments` lived in `frey-tools`, and `frey-mcp` does not
depend on `frey-tools`, so the agent loop validated and the server did not — the same toolset
behaving differently depending on who called it, which is the exact divergence "one catalog, many
presentations" is supposed to prevent.

The direction is what makes it bad rather than merely inconsistent: of the two callers, the remote
one is **less** trustworthy, so if either surface were going to skip validation it should not have
been that one.

Fixed by moving the validator to `frey-core`, next to the types it operates on, where every dispatch
surface can reach it. Re-exported from `frey-tools` so nothing downstream broke.

**The general lesson:** any check that exists on one dispatch path is a bug on every other one.
There are now three surfaces that call tools — the agent loop, the MCP server, and multi-agent
spawn — and nothing structurally stops a fourth from being added without the checks. A shared
`dispatch` function that all surfaces must go through would; that is the real fix and it is not done.

---

## Shape problems the demos hit, not yet fixed

### D3 — `Toolset` and `ToolHost` are two traits for one job. Severity: medium.

The agent loop takes a `ToolHost`; everything else takes a `Toolset`. Every application that has a
toolset and wants to run an agent writes the same adapter, and thicket's is representative:

```rust
impl ToolHost for Host {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let cx = StepCx { /* four fields invented from nothing */ };
        pollster::block_on(self.0.definitions(&cx)).unwrap_or_default()
    }
    ...
}
```

Two problems beyond the boilerplate. `ToolHost::definitions` is **synchronous** while
`Toolset::definitions` is async, forcing a `block_on` inside what may already be an async context.
And the error is swallowed by `unwrap_or_default()`, because a sync infallible signature leaves
nowhere to put it — so **a toolset that fails to list its tools silently presents none**, and the
agent confidently reports it has no way to do the task.

That last part is a genuine violation of the project's own rule that nothing degrades quietly, and
it is caused by a trait signature rather than by any code that was written carelessly.

**Fix:** either a blanket `impl<T: Toolset> ToolHost for T`, or make `ToolHost::definitions` async
and fallible. The second is better; it is a breaking change and should happen before 0.2.

### D4 — `StepCx` must be invented at every boundary. Severity: low, hit twice.

Both demos and Frey's own MCP server construct a `StepCx` with `tokens_available: u32::MAX`, an
empty `task`, and a placeholder `RunId`, because a server genuinely has none of those. Every caller
outside an agent loop will write the same placeholder, which means the type is asking a question
that has no answer at that point.

**Fix:** `StepCx::unbounded()`, so the placeholder is written once and named for what it is.

### D5 — Definitions are fetched twice per MCP call. Severity: low.

Validating a call means asking the toolset for its definitions to find the schema; `tools/list` asks
again. For a toolset backed by anything remote that is two round trips where one would do.

Not simply wrong — a definition can legitimately change between steps, and caching would validate
against a stale schema. But a `Toolset::definition(name)` method would let an implementation answer
one lookup cheaply.

### D6 — Small API friction. Severity: cosmetic, but it is the first ten minutes.

- `frey_core::…` does not resolve downstream; it is `frey::core::…`. The prelude covers most cases,
  but the paths that are not in the prelude cost a compile cycle each to discover. One line in the
  getting-started docs fixes this.
- `Provenance` has public fields (`origin`, `via`) while looking like a type with accessors;
  `.source()` was the natural guess.
- `ToolErrorKind::InvalidArgs`, not `InvalidArguments`. The compiler suggests the right name, so
  this costs nothing — noted only because it happened three times.

---

## What held up

Recorded because a findings file that lists only complaints is not an honest account of a day.

- **`ToolError` with guidance is the best thing in the framework.** Both demos independently ended
  up leaning on it, and watching a small model receive `"there is no memory with id m404"` plus
  `"call recall to find the right id"` and correct itself is the clearest argument for the design.
- **One toolset really did serve two surfaces.** thicket's `Memory` is exposed over MCP and called
  in-process with no second registration and no drift. That is the central claim and it held.
- **The transport split paid off immediately.** switchboard's entire HTTP layer is one POST route,
  because `Server::handle` takes a `Value` and returns one. No protocol logic in either demo.
- **Statelessness survived a hostile test.** Round-robining an approval handshake across two
  independently constructed replicas passed with no change to Frey.
- **Argument validation caught a bug in thicket** the day it was added — `remember` with no
  arguments had been filing an empty memory.
- **`Caller::Code { runner }` was exactly right.** abacus marks script-issued calls with it in one
  line, which is what lets the tool layer enforce caller policy client-side rather than trusting the
  provider's advisory `allowed_callers`.
- **`generate_api` produces genuinely good TypeScript** — good enough that the model concludes it
  may write TypeScript, which is a compliment the feature cannot use.

---

## D7 — `Strategy::Local` cannot be fulfilled by a user, and the docs imply it can

Found by abacus, and it is the most consequential thing in this file because it changes a claim
rather than fixing a defect.

Code mode's missing engine is documented as a cost-benefit call: delegation is correct for
Anthropic, and a JS runtime in every build is a cost most users would pay for nothing. True, and it
reads as *"supply your own executor if you need local execution"*. abacus tried.

**Models will not write a restricted mini-language.** Two presentations of the same three-form
language, both refused:

| Presentation | What the model wrote |
|---|---|
| Frey's `generate_api` TypeScript declarations | `let eu_orders = []`, `orders[0]`, `region === "eu"`, a ternary |
| The same tools as example calls in the target syntax | `filter(orders, …)`, `first(open_orders)` — functions that do not exist |

The second refusal is the informative one. The prompt showed only the legal forms and said in
capitals that there are no loops, no `if`, and no arithmetic; the model invented collection helpers
anyway, because a language with a list in it ought to have them. Two presentations isolates the
variable: this is not a prompting problem that a better prompt fixes.

So the honest conclusion is stronger than the recorded one. Delegation and a real embedded engine
are the only two options; "bring your own small executor" is not a third. The decision to defer the
engine stays correct — the documentation was leaving a door open that is not there.

**Done:** the README, the FAQ, and the caching doc now say delegation is the only working path.
**Still to do:** `Strategy::Local` should carry that in its own doc comment, or be removed until an
engine exists.

### A related framing problem

abacus also measured where the saving is. On twelve tools the typed API is 554 tokens against the
tool block's 700 — 21%, on the part of the prompt that is cached anyway. Over the same task, **15
tool results** crossed the context window across 5 round trips.

Frey's docs described code mode as a presentation of the catalog, which is what the code does and
the less important half. It is *for* keeping intermediate results out of the window. Now fixed in
`docs/context-and-caching.md`.

---

## Ranked, for whoever picks this up next

1. **D3** — the `ToolHost` signature silently swallows a toolset failure. Breaking change, worth
   making before 0.2, and the only item here that can lose information at runtime.
2. **The general lesson under D2** — one shared dispatch path, so a new surface cannot skip the
   checks. Nothing currently prevents the next one from repeating D2 exactly.
3. **D7** — `Strategy::Local` should state that a local executor means a real engine, or go away.
   Currently the type promises something no user can build.
4. **D4 and D5** — small, additive, no reason to wait.
5. **D6** — documentation.
