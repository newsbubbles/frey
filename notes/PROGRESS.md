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
| M9 MCP | ✅ done | stateless client, legacy shim, defensive re-sort |
| M10 discovery | ✅ done | regex mirroring provider semantics, BM25 |
| M11 sandbox | ✅ done | fail-closed; runtime probing; found a real scope bug |
| M12 built-in tools | ✅ done | argv-only shell, egress allowlist, workspace paths |
| M13 skills | ✅ done | ladder, trust boundary, no self-granted capabilities |
| M14 code mode | ⚠️ partial | codegen + bindings + delegation; embedded engine deferred |
| M15 multi-agent | ✅ done | grant monotonicity, backpressure rule |
| M16 A2A | ✅ done | agent cards, 8-state lifecycle, stream rules |
| M17 harness | ⏳ in progress | |
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

## M9–M10 — MCP and discovery

ADR-0020 records a change of plan: the MCP client is implemented directly rather than on `rmcp`. The
wire format is JSON-RPC over the HTTP client that already exists; what Frey actually needed was the
*policy* around it, which no SDK provides — negotiation, catalog caching, defensive re-sorting,
namespacing, and mapping `input_required` onto the one `NeedsInput` type. Implementing it directly
makes the whole client testable against a fake transport, with no network and no server.

An MCP server is an untrusted party, and the client is built that way:

- **Listings are re-sorted.** The specification asks servers to be deterministic to protect prompt
  caches. A server that ignores it would churn the tool block's hash every turn, and the cost lands
  on the client — so the client defends itself.
- **Freshness hints are capped** at an hour. A `ttlMs` of a year would pin a stale catalog.
- **Catalogs are private by default.** Sharing across principals when the server did not say it was
  safe would leak one user's tools to another.
- **Tool results are `Untrusted` by construction**, with provenance recording which server and tool.
- **A method-not-found for `server/discover` is negotiation, not failure** — it is how a
  pre-stateless server identifies itself, and treating it as an error would make every existing
  server unusable.

Discovery mirrors the provider-native semantics deliberately, down to the 200-character regex limit
and the 5-result default, so a query behaves the same whether Frey ran it or delegated it. Both
strategies index the same four fields the provider does: name, description, argument names, and
argument descriptions. The BM25 test that finds `db_query` from "postgres dialect" only passes
because that tool's parameter is documented — which is the concrete reason an undocumented parameter
is a defect rather than a style preference.

## M11–M12 — security

A real security bug surfaced here, from a test rather than a review. `PathScope::new(["./"])`
normalised to `"/"`, so a policy that read as *the workspace* actually granted the entire
filesystem. Nothing looked wrong — which is what makes it the worst kind of bug, and why the fix
carries a permanent regression test rather than a comment.

Two modelling clarifications also came out of building it:

- **`ProgramAllowlist` is enforced by Frey, not by the kernel.** It belongs in the baseline every
  platform reports, because it is enforced by refusing to spawn. It is also the control that matters
  most: a program that never starts cannot escape anything.
- **Partial confinement is reported precisely.** Landlock ABI 1 scopes the filesystem but not ports.
  Reporting that as "unavailable" would push an operator toward disabling confinement entirely, so
  the refusal names exactly which control is missing.

The probing logic is a pure function of a detected ABI level and an `lsm=` flag, so the *degraded*
paths — which a healthy CI machine cannot reproduce by running — are ordinary unit tests on every
platform. The subtlest case has its own test: a kernel with Landlock compiled in but not enabled at
boot enforces nothing, and the message says exactly which boot parameter to change.

The built-in tool validators demonstrate the shape rather than just providing utility. `sh -c
"r''m -rf /"` fails on `sh`, before the payload is ever considered, because the program is compared
as a whole argv element and there is no command string to obfuscate. `https://api.github.com@evil.test/`
is refused because it reads as GitHub and resolves elsewhere. An environment variable that looks
like a credential is refused outright, since a sandbox never holds a secret.

## M13–M14 — token efficiency

Skills share the selector, budget and search index with deferred tools, because they are the same
mechanism at a different altitude. Two properties get tests rather than paragraphs: the index rung
stays small next to a body of the size the format actually recommends, and twenty skills do not cost
twenty bodies at startup. The fixture is deliberately a realistic length — a two-line skill would
make the ladder look pointless by measuring nothing.

Skills are a trust boundary and are built that way. A skill outside a trusted root reaches the prompt
as low-integrity text whatever it says about itself, and **a skill cannot grant itself
capabilities** — anything ungranted is refused at load, not prompted for mid-run, because a mid-run
prompt is exactly where an injected instruction would like to be answered.

### Code mode is partial, deliberately

Shipped: the typed API generator, the capability binding model, and the strategy that delegates to a
provider which can already run the script. Not shipped in the default build: an embedded JavaScript
engine.

The reasoning, stated rather than hidden. Anthropic's programmatic tool calling runs the script on
their side, so for that provider the correct implementation *is* delegation, not a second sandbox.
And the generated surface earns its place regardless — it is emitted even when code mode is off,
because it doubles as the description corpus tool search indexes. Pulling a JavaScript runtime into
every build of a Rust agent framework is a cost every user pays for a feature most will delegate, so
it belongs behind a feature flag. That is a scope reduction against the plan, and it is recorded
here rather than quietly absorbed.

## M15–M16 — many agents

The multi-agent module is mostly about saying no. No graph DSL, no supervisor tree, no swarm
metaphor — those are where claims stop being falsifiable. What ships is one invariant and the
plumbing around it.

**Capabilities only narrow.** A child's grants are always a subset of its parent's, checked at spawn
rather than at use, because by the time a capability is exercised the decision has been made. A
descendant of an empty grant set can acquire nothing however deep in the tree it sits, and there is
a test at depth to say so. Untrusted input also flows downward: a parent that has read a fetched
page cannot hand a child a clean slate by summarising, since the summary derives from that page.

The backpressure rule has one place in the codebase: presentation deltas may be dropped, semantic
events may not — even when honouring that means exceeding the soft capacity. Going over budget is
recoverable; a transcript that lies is not. Dropped deltas are counted, so "the UI looked laggy" is
diagnosable from a number.

A2A confirmed ADR-0010 rather than testing it. Its `INPUT_REQUIRED` and `AUTH_REQUIRED` are
*interrupted, non-terminal* states, exactly MCP's multi round-trip result and AG-UI's interrupt, and
projecting one onto the other took a dozen lines. Had A2A been added after the loop was built around
a different shape, that would have surfaced far too late to be cheap.

One point worth stating plainly: **a verified signature on an agent card does not make its text
trustworthy.** Verification changes who is responsible for the text, not whether it can be obeyed.
The test that pins this uses a signed card carrying an injected instruction, and asserts the text
arrives indexed and low-integrity. An *invalid* signature is refused outright — worse than unsigned,
because someone tried.
