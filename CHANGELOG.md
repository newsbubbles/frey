# Changelog

All notable changes to Frey are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) — with the caveat that `0.x` makes no
stability promise, and this one means it.

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
