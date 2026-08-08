# Frey — Milestone Progress

Running log against [BUILD-PLAN.md](BUILD-PLAN.md). Updated as each milestone lands, not before.

| Milestone | State | Notes |
|---|---|---|
| M0 workspace + taint | ✅ done | ADR-0011 resolved by prototype |
| M1 core vocabulary | ✅ done | serde tagging landmine found |
| M2 core contracts | ✅ done | dynosaur `Send` finding, ADR-0006 amended |
| M3 frey-testkit | ✅ done | landed before the first provider, per plan |
| M4 cache planner | ✅ done | two bugs found by property tests |
| M5 providers | ✅ done | SSE keepalives, 402 fatal, encrypted reasoning replay |
| M6 macros | ✅ done | parameter doc comments become schema descriptions |
| M7 tool tower | ✅ done | policy, approval, redaction, truncation, registry |
| M8 agent loop | ✅ done | first end-to-end agent; journal, replay, ledger |
| M9 MCP | ⏳ in progress | |
| M10 discovery | ⬜ | |
| M11 sandbox | ⬜ | |
| M12 built-in tools | ⬜ | |
| M13 skills | ⬜ | |
| M14 code mode | ⬜ | |
| M15 multi-agent | ⬜ | |
| M16 A2A | ⬜ | |
| M17 harness | ⬜ | |
| M18 CLI | ⬜ | |
| M19 release | ⬜ | |

---

## M0–M4 — foundations and the wedge

Four findings worth carrying forward:

1. **serde internal tagging cannot carry `RawValue`.** `#[serde(tag = "...")]` buffers through
   `Content`, which cannot represent the newtype trick `RawValue` uses. It compiles and then
   destroys byte fidelity at runtime. `Item` and `EventKind` are externally tagged with a named
   regression test.
2. **`dynosaur`'s default erasure is not `Send`.** Trait methods declare
   `-> impl Future<..> + Send` explicitly, which fixes it and states the requirement in the API.
3. **Cache lifetimes must be positional, not by segment kind.** Nothing orders segments by kind, and
   a short-lived mark before a long-lived one is a 400.
4. **"No marks" is ambiguous.** The provider caching automatically and nothing being cacheable are
   different answers to the same question, and a developer needs to know which.

## M5 — providers

Each adapter splits into a pure `Dialect` (no I/O) and one shared `HttpProvider`. That split means
the entire wire mapping — every piece of provider nuance — is testable without a network, a key, or
a mock server, and retry policy is written once instead of three times.

Four things the tests pin down, each a way to lose money or correctness quietly:

- **SSE keepalive frames.** A bare `.json()` on an HTTP 200 intermittently throws because comment
  frames precede the body. The decoder handles comments, chunk boundaries mid-UTF-8, CRLF, and a
  final frame with no trailing blank line.
- **402 is fatal.** `is_fatal` short-circuits the retry loop, so exhausted credit stops the run
  instead of turning every remaining turn into a silent no-op.
- **Encrypted reasoning is replayed verbatim.** The request asks for it (`store: false` plus the
  include) and the response round-trips `ProviderCarry` byte for byte. Dropping it is silent, makes
  answers worse, and costs money to regenerate.
- **Token accounting differs by vendor.** Anthropic's `input_tokens` excludes cached tokens;
  OpenAI's includes them. Getting that backwards skews every cost figure, so both directions have a
  test asserting the total matches what the provider reported.

`OpenAiChat` deliberately has no `Default`: an endpoint with an empty provider id would produce
ledger entries and audit records naming nothing.

## M6–M7 — macros and the tool layers

`#[frey::tool]` turns a plainly-written async function into a tool. The design decision worth
recording is that **parameter doc comments become schema descriptions**, because tool search matches
on argument names and descriptions — so an undocumented parameter is lost search surface, and a tool
becomes measurably harder to find once the catalog outgrows the context window. Writing the doc
comment and making the tool discoverable are the same act, which is the only way that habit survives
a deadline. A tool with no description at all fails to compile, with an error that says why.

The layers enforce three things the provider will not:

- **Caller policy.** Anthropic document `allowed_callers` as guidance to the model, not a boundary.
  Enforcing it here is what turns a "code-only" marking from decoration into a rule.
- **Approval prompts show the literal action**, never a natural-language summary — a summary is
  exactly where an injected instruction hides from the person approving it.
- **Risk comes from the declaration.** A tool's own account of how dangerous it is, or the model's,
  is not evidence.

The registry uses a `BTreeMap` rather than a `HashMap` on purpose: the tool block is the stable cache
prefix, so iteration order must not depend on insertion order or a per-process hash seed. A name
collision is an error at registration naming both sources, rather than one tool silently shadowing
another depending on load order.

## M8 — the agent loop

🎯 **First end-to-end agent.** Model, tools, context plan, cost ledger, journal.

Each turn is the same five steps and the order is not negotiable: segment the prompt, fit it to the
budget, plan the cache against what survived, call the provider, then run whatever tools it asked
for through the layers. The cache plan has to see the final segment list, and the layers have to see
a call before anything executes.

The journal records only what is non-deterministic — model responses, tool results, supplied input.
Prompt assembly, budgeting and cache planning are pure functions of those, so recording their output
would let a real change hide behind a stale recording.

Replay diverges loudly at the exact step, naming what was recorded and what the run produced. A
replay that quietly adapts is worse than none, because it produces confident results about a run
that never happened.

Everything that could degrade quietly instead produces something the caller can see: eviction, cache
churn, truncation (with the withheld byte count, and a note telling the model how to get the rest), a
missing capability, an output cap, and a fatal provider failure that ends the run rather than
retrying into silence.

One documented limit: the replay fingerprint compares request *shape* — model, turn count, tool
names — not full prompt text. A journal storing every prompt verbatim would be enormous. The test
that pins this also documents what it therefore cannot catch.
