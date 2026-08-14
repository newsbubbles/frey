# Changelog

All notable changes to Frey are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) — with the caveat that `0.x` makes no
stability promise, and this one means it.

## [0.2.1] — 2026-08-14

**Frey now knows what it costs.** `0.2.0` shipped with no performance numbers at all and a
`claims.toml` with no performance rows — which at least meant nothing was overclaimed, and meant
nothing was known either. This release measures it, logs it per turn, and makes it readable back.

It also retracts three claims `0.2.0` made about the MCP client, one of which was in that release's
own notes.

### Added

- **`EventKind::TurnFinished` carries a `TurnTiming`** — seven phases per turn, into the journal
  *and* the tracing span, on every turn including the ones that run tools. Five phases are Frey's;
  two are not, and keeping those apart is the whole point of the type.
- **`frey timings <journal.jsonl>`** reads it back across a recorded run, in medians. A journal
  written before this existed reports no data and exits non-zero rather than printing zeros.
- **Three examples that measure rather than demonstrate**: `turn_timing`, `prompt_scaling`, and
  `concurrency`, plus `live_concurrency` for a real provider.

### Fixed

- **Every run in a process shared one run id.** `RunId` was `format!("{session}-run")` and
  `Agent::new` defaults the session to the literal `"default"`, so any caller who did not know to
  set one got the same id for every run — concurrent or not. Nothing fails at the time; the run id
  is the primary key for which journal belongs to which run, which `RunStarted` a frontend is
  following, and which record an incident refers to. Now session, process id and a counter, with
  the not-globally-unique bound stated on the function. `RequestFingerprint` does not include the
  run id, so replay is unaffected. **Found by a concurrency test written to measure something else**
  ([I-012](notes/INCIDENTS.md)).
- **`provider.complete(request.clone())` billed the clone to the provider.** The argument is
  evaluated before the future starts, so cloning the entire prompt — every turn — was counted as
  waiting for the network. Hoisted out and attributed to assembly, where it belongs. On a 200-tool
  catalog that is about 1.6 ms a turn that had been filed under somebody else's name.
- **A doc comment naming `SandboxBackend::spawn`,** a method that does not exist, in the crate about
  failing closed.

### Changed

- **`TurnTiming::overhead_permille` is now `overhead_ppm`.** Per-mille read `0` on every row of the
  first measurement against a live provider — 30 µs of framework against 800 ms of network rounds to
  nothing at one part in a thousand. A column of zeros is not a result; it is a unit that cannot
  express the result, and the fake-provider sweep could never have shown it because its latency was
  a constant chosen by the author.

### Retracted

Three claims `0.2.0` made, withdrawn ([I-011](notes/INCIDENTS.md)):

- **`McpClient` ships no `Transport`.** Both implementations in the repository are inside
  `#[cfg(test)]`. There is no stdio transport and no HTTP transport; connecting to a real server
  means writing one.
- **There is no client-side shim for older servers.** `negotiate()` identifies a pre-stateless
  server and writes `stateless: false` into `ServerIdentity`, and nothing reads that field —
  `initialize` is never sent. Since 0 of 6 reachable third-party servers speak the stateless
  revision, that is every real server tested.
- **`mcp.works-with-servers-frey-did-not-write` was `operated` on evidence that never ran the code
  it was about.** `cargo xtask conformance` hand-rolls JSON-RPC and `xtask` does not depend on
  `frey-mcp` at all. It measured the ecosystem, which is real and kept as
  `mcp.the-ecosystem-still-needs-a-handshake`. It measured nothing about Frey. **This claim appeared
  in the `0.2.0` release notes**, which is why it is repeated here rather than only in the file.

The **server** direction is unaffected: it is real, tested, and includes its own `initialize` shim
for pre-stateless *clients*, which is a different mechanism that does exist.

### What the measurements found

- **~12 µs of framework overhead per turn** warm, on an empty catalog. The first turn in a process
  costs ~280 µs, roughly 95% of it lazy initialisation and first-touch page faults; both are now
  reported separately rather than one standing for the other ([I-013](notes/INCIDENTS.md)).
- **~16 µs per tool per turn** — about **3.3 ms on a 200-tool catalog**, close to linear. Twenty-five
  turns of accumulated history moved it not at all. The catalog is re-segmented, re-hashed and
  re-cloned every turn; history grows slowly and the budgeter is already evicting it. **If you are
  worried about a long conversation, worry about the catalog instead.**
- Roughly half of that is cloning tool definitions into the request, which cannot change within a
  run. Recorded, not fixed.
- **Median overhead does not degrade from 1 to 1,024 concurrent agents** on one shared adapter. The
  p99 runs 40–60× the median and is noisy between repeats; that is written down rather than omitted,
  and it is not explained.
- **Against a live provider: 16–51 parts per million of a real turn**, and **32 concurrent agents
  with zero failures** on one shared adapter. 106 paid requests across two flash-tier models,
  **$0.00015**. Dated record in `notes/perf/live-concurrency.jsonl`.

### The claims table

**61 rows: 29 settled by a named test, 2 operated, 6 tested-only, 14 unevidenced, 10 retracted.**
Ten retractions against twenty-nine settled claims is the ratio worth looking at, and it is the
first release where the retracted column moved more than the settled one.

## [0.2.0] — 2026-08-14

**The release where a real caller arrived.** `0.1.0` and `0.1.1` were written against scripted
models and the author's reading of provider documentation. Since then Frey has been driven by three
demonstration projects, a live tier sweep across a dozen models, an external reviewer, and one
unreleased private application putting thousands of short sessions through it — live routing, real
money. Nearly everything below was found by one of those rather than by review.

One shape recurs so often it is worth naming up front, because it is the defect class this release
is mostly about: **a declared capability, with documentation and types and consumer-side tests, and
no producer on the path that mattered.** A dialect that says it reports cost and never asks for it.
A capability flag for reasoning with no decoder behind it. Events defined, translated, and tested in
isolation that nothing ever emitted. A cache planner whose placements no adapter applied. Each one
is unfalsifiable from the consuming side, which is why they all survived a code review and a
capability audit. `cargo xtask producers` now looks for it on every push.

### Breaking

- **`ToolHost::definitions` is `async` and fallible.** It was sync and infallible while
  `Toolset::definitions` was async and fallible, so every adapter between them ended in
  `unwrap_or_default()` — a toolset that failed to list its tools presented *none*, and the agent
  confidently reported it could not do the task. A quiet degradation caused by a trait signature.
  Held out of 0.1.1 on purpose because the fix contained a real question; resolved as: `Err` fails
  the run, a **reduced** catalog does not, and an empty one is reported whatever produced it. The
  catalog is now fetched once per run rather than per turn.
- **`RunError::{Provider, Budget, ToolCatalog, TurnLimit}` carry the journal.** `TurnLimit` said
  "look at the transcript for a loop" and threw the transcript away. `Journal::totals()` prices a
  run that ends without a `RunOutput`, because a turn limit reporting zero cost is how a runaway
  stays invisible in a ledger. `RunError::ToolCatalog` is new.
- **`OpenRouter` carries explicit-cache state.** `OpenRouter::new()` is unchanged;
  `with_explicit_cache()` opts into placing breakpoints for `anthropic/*` upstreams.

### Fixed

**The budgeter evicted and the loop sent everything anyway.** `Budgeter::fit` computed what to drop,
announced what it had freed, and `fitted.evicted` was read nowhere in the crate — the request went
out untrimmed. Nothing failed: the run succeeded and the freed tokens were billed, until a prompt
overshot far enough for a provider to refuse it, from a framework that had just said it had made
room. This is the wedge, and it is the worst defect this project has recorded.

**Three cache warnings that could not fire, and marks that never reached the wire.**

- `CachePlanner::plan` returned before churn detection whenever the breakpoint budget was zero, and
  an automatic-caching provider has a budget of zero *by definition*. So `CacheChurn` and
  `BelowMinPrefix` were structurally unreachable on OpenRouter — the only dialect in production use.
  Churn is a property of the prompt, not of who places the breakpoints; it is detected first now, and
  the minimum-prefix check runs against the leading stable run, which is what a
  longest-common-prefix cache can actually reuse.
- **Anthropic honoured only the last mark**, collapsing a four-breakpoint plan to one, dropped it
  entirely when the system prompt was empty, and never marked the tool block — three lines below a
  doc comment saying that it did.
- **OpenAI Responses declared `explicit_available: true`.** That API has no breakpoint mechanism;
  `prompt_cache_key` is routing affinity. The planner placed a mark on every request and the adapter
  discarded every one, silently, while the plan reported it as placed.

Both were found by `frey_providers::marks::survey`, which encodes a representative request through
every dialect and *counts the markers that come out* — the question nobody had asked, which is
whether what the planner produces appears in the bytes we send.

**Everything else, each with a test that fails without the fix:**

- **OpenRouter never asked for the cost it decodes.** `encode` never set `usage.include`, so cost was
  always absent and every ledger entry read "the provider did not say". The decode half had a test;
  the encode half did not.
- **`Arc<P>` was not a `ModelProvider`.** The shared-adapter pattern is documented in three places
  and did not compile.
- **`Warning` had no `Display`.** "Nothing degrades quietly" is only true if the diagnostic reaches
  a person, and eight `non_exhaustive` variants cannot be matched downstream.
- **The agent loop emitted no tool-call events at all.** `ToolCallStarted`/`Finished`/`Failed` were
  defined, translated to AG-UI frames, and tested in isolation. Nothing produced one.
- **A reasoning model looked like a broken one.** `decode_chat` ignored `message.reasoning`, so a
  model that spent its budget thinking decoded to *zero* items; the loop read that as the model
  having finished and ended the run. It cost an afternoon of tier selection —
  `openai/gpt-oss-120b` appeared to go 4/5 to 0/5 on unchanged code.
- **A tool call with no arguments does not have null ones.** Absent or unparseable
  `function.arguments` decoded to `Value::Null` and re-encoded as the string `"null"`, which
  strict upstreams answer with HTTP 400 for every remaining turn.
- **Argument validation lived in `frey-tools`,** so the agent loop validated and the MCP server did
  not — the same toolset behaving differently depending on who called it, with the *less* trusted
  surface being the one that skipped. Moved to `frey-core`.
- **No cap on tool calls per turn.** One model emitted ~145 in a single response and the loop ran
  every one: 10× the cost, wrong answer. `max_tool_calls_per_turn` defaults to 32 and refuses the
  excess individually, because a dropped call is indistinguishable to the model from one that
  returned nothing.
- **No HTTP timeout**, against a client whose default is to wait forever. Two clocks, because slow
  and hung are different failures.
- **A 200 with an error body** reported "response has no `choices`", discarding the sentence saying
  whether it was moderation, rate limiting, or an outage.
- **The loop's own error guidance pointed at an affordance that does not exist** — "or search for
  one", when nothing in the loop consults tool search.
- **`ContentHash` deserialize demanded a borrowed `&str`** and so could not be loaded from a file,
  which is the only place a journal lives.
- **The event stream did not close on every exit.** `RunFinished` is now emitted on every path with
  the totals attached, duplicates are dropped keeping the first, and `Warning::EventsDropped` says
  when the channel overflowed.

### Added

- **Frey can *be* an MCP server** (`frey_mcp::Server`), not only call one — the same `Toolset` an
  agent calls in-process, served over the wire from one registration. No transport: `handle` takes
  a JSON value and returns one.
- **`ToolCx::resume`** closes the `input_required` handshake, which is the mechanism that makes a
  stateless server possible at all. A retry carrying no answers asks again rather than proceeding:
  an absent answer is not a yes.
- **`AgentCli`**, an `AgentProvider` that runs the vendor's own binary. Frey never stores, mints,
  refreshes or replays a vendor token — there is nowhere in the public API to put one, and a test
  asserts nothing credential-shaped reaches the argv, because Anthropic's usage policy prohibits
  third-party use of subscription credentials.
- **`Agent::cache_key`** and **`HttpProvider::with_client`**, both from the first external review:
  many short sessions sharing one persona need one cache key, not one per session, and one
  `reqwest::Client` per agent multiplies connection pools until it fails as socket exhaustion.
- **Replay is reachable from the loop** as `Replaying`, an ordinary `ModelProvider`. Fingerprints
  carry a content hash now, and a journal written before that reports `Divergence::Unknown` rather
  than a match.
- **Estimator reconciliation.** Every turn compares `len / 4` against the provider's own count and
  warns past 25%.
- **`Request::mark_placement()`**, which resolves segment ids to wire positions once, for every
  adapter — and `frey_providers::marks::survey`, which measures what each dialect emits.
  `frey doctor` reports the split from that measurement rather than from a table.
- **`EventKind::TurnStarted`**, **`Warning::RouteChanged`** (model substitution), and
  **`Warning::EventsDropped`**.
- **`cargo xtask check`** — a producer lint over `Warning`, `EventKind`, `RunError`, `Item` and
  `Effect`; a **`claims.toml`** checker that resolves test names and dated evidence rather than file
  paths; and a **conformance sweep** that connects Frey's MCP client to ten third-party stdio
  servers for free. All three run in CI.

### Corrected

Claims this repository made and now withdraws, kept visible rather than deleted:

- **`Strategy::Local` has nothing behind it.** Code mode without an embedded engine read as "supply
  your own executor". You cannot: handed a bespoke grammar stating in capitals that there are no
  loops and no arithmetic, models invent `filter()` and `first()` anyway. Two presentations, two
  refusals — a prior about what code is, not a prompting problem. The deferral was right; the
  documentation around it was not.
- **`Response::provider` is the adapter id, not the upstream.** No decoder reads an upstream name
  out of a response body, so `RouteChanged` detects *model* substitution — which changes the
  tokenizer and the price — and cannot see a silent move between two hosts of the same model.
- **0 of 6 reachable third-party MCP servers speak the `2026-07-28` stateless revision.** The shim
  for older servers is not a compatibility nicety; it is the only code path that has ever worked
  against a server Frey did not write. Recorded in `notes/conformance/`, re-runnable for free.

### The part that is not code

- **[`claims.toml`](claims.toml)** — every claim in the README and docs, with a status and whatever
  stands behind it, checked on every push. 53 rows: 28 settled by a named test, 1 operated, 3
  tested-only, 14 unevidenced, 7 retracted. `operating.unattended` is unevidenced; 498 tests pass
  and nobody has run Frey unattended, and the file exists so those cannot be confused.
- **[`notes/INCIDENTS.md`](notes/INCIDENTS.md)** — ten entries, each with a `found_by` field saying
  whether an instrument caught it or a person did. The ratio is the measurement: a project with no
  incidents is indistinguishable from a project with no instruments.

## [0.1.1] — 2026-08-09

`0.1.0` was tagged before its first public CI run, which then failed on three things worth fixing
rather than hiding. The tag is left where it is; this is the first release with a green build.

### Fixed

- **Compile-fail tests no longer pin diagnostic text.** The `trybuild` suite asserted the exact
  rustc wording, and those expectations were generated on Windows, so the suite failed on Linux for
  a reason unrelated to what it tested. Replaced with `compile_fail` doctests, which assert the same
  property and let the compiler phrase the refusal. The CI workaround that skipped the test on two
  of three platforms is gone with it — a test worth running is worth running everywhere.
- **A clippy lint** that a newer toolchain catches and 1.94.1 does not. Fixed rather than allowed.
- **No wildcard dependencies.** Path dev-dependencies without a version resolve as `*`, and a crate
  intending to be published cannot depend on any version of anything.
- **`CDLA-Permissive-2.0` added to the licence allowlist**, deliberately and with a note: it arrives
  with the Mozilla CA bundle that rustls ships.

## [0.1.0] — 2026-08-09

First release. Complete against the build plan in `notes/BUILD-PLAN.md`, with two scope reductions
recorded below rather than quietly absorbed.

### The framework

- **Context economy.** A cache planner that is a pure function of segments, last turn's hashes, and
  provider capabilities. It knows Anthropic's four breakpoints and per-model minimum prefix,
  OpenAI's automatic caching and routing key, and OpenRouter's per-upstream split. It refuses to
  place a breakpoint on a segment that changed last turn, catches a prefix below a model's minimum
  (which providers accept and silently do not cache), and catches a turn that exceeds the 20-block
  lookback. Property-tested across seven provider profiles.
- **Information-flow labels as types.** Everything from outside is `Tainted`. Passing it to
  something needing trusted input is a compile error, proved by a `trybuild` suite. Raising
  integrity is audited with its call site, and is usually done by a parser.
- **Errors typed by audience.** Model, operator, and user are three fields; tests assert operator
  diagnostics can reach neither the context window nor a browser.
- **Deterministic replay.** Every non-deterministic effect is journalled; replay diverges loudly at
  the exact step rather than adapting.
- **Capability scoping.** No ambient authority, monotonic narrowing across spawn trees, and the
  Rule of Two as a session invariant that survives a restart.
- **A sandbox that fails closed**, reports what it actually enforced, and detects Landlock rather
  than assuming it.
- **MCP `2026-07-28`** — the stateless revision — with a shim for older servers, and defensive
  handling of a server as the untrusted party it is.
- **A2A v1.0** and **AG-UI**, sharing one `NeedsInput` type with MCP's retry pattern.
- **Providers**: Anthropic, OpenAI Responses, OpenRouter, and dialects definable in configuration.
- **`frey-testkit`**, published so you can test your agent the way Frey tests itself.

### Known limitations

- Nobody has run this in production, including its author.
- Code mode ships the typed API generator, capability bindings, and provider delegation. There is no
  embedded JavaScript engine in the default build.
- The Landlock ABI level is not yet detected by syscall; `doctor` reports the conservative answer
  rather than a number that might be wrong.
- No live-provider test corpus. Everything is exercised against a scripted model and recorded shapes.
- Cost figures are estimates everywhere except OpenRouter.

### Notable findings, recorded in `notes/PROGRESS.md`

- serde's internal tagging silently corrupts `RawValue`, which would have destroyed byte fidelity at
  runtime while compiling cleanly.
- `dynosaur`'s default erasure is not `Send`, which would have surfaced at multi-agent spawning
  rather than at its cause.
- `PathScope::new(["./"])` normalised to `/`, silently granting the whole filesystem to a policy
  that read as "the workspace". Found by a test, fixed with a permanent regression test.
- The adversarial re-check of the project's own positioning found both partial prior art and a rule
  missing from the planner. Both are recorded in `notes/research/03` §5.
