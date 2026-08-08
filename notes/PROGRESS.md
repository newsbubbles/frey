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
| M6 macros | ⏳ in progress | |
| M7 tool tower | ⬜ | |
| M8 agent loop | ⬜ | |
| M9 MCP | ⬜ | |
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
