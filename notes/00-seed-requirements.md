# Frey — Seed Requirements

> Captured 2026-08-08 from the founding brief. This is the *source of truth for intent*.
> Everything downstream (research notes, architecture, ADRs) must trace back to a line here.

## One-line

**Frey** is a Rust-native agent framework *and harness toolkit*: production-grade agents in a few
lines, MCP-native tooling, multi-provider, security-first, token-efficient at large tool counts.

## Non-negotiable requirements (verbatim intent)

### R1 — MCP-native tooling
- Built on the **latest** Model Context Protocol, including **stateless servers**.
- Tools are MCP-shaped by default; local/native tools and MCP tools live in one registry.

### R2 — Trivial to start a versatile agent
- "Very easy to start up a quite versatile agent." Small API surface for the 90% case,
  full control available underneath. No mandatory ceremony.

### R3 — Provider adapters, config-extensible
- Direct, first-class from v1: **OpenRouter, OpenAI, Anthropic**.
- Adapter layer extensible **via configuration**, not just via code.
- Model the surface on pydantic-ai's provider/model split; handle **provider nuance**
  (reasoning params, caching semantics, tool-call formats, strict schema modes, etc.).

### R4 — Harness-first, not just framework-first
- 2026 reality: *harnesses* (Claude Code, Codex, Cursor-likes) are the popular shape.
- Frey must make writing a **harness** as natural as writing an agent.
- Offer **Claude SDK / Codex SDK**-style adapters so users can ride **existing subscriptions**
  to frontier models instead of paying per-token.

### R5 — Real toolsets in the box
- Including an **actually secure shell tool** (not a `Command::new("sh")` wrapper).
- Toolsets should be composable, permissioned, and auditable.

### R6 — Tool presentation + discovery at scale
- Treat "how the model sees tools" as a first-class design problem.
- **Tool search / tool discovery** so huge toolsets don't blow up context.
- Progressive disclosure: definitions loaded on demand.

### R7 — Token efficiency
- Caching as a designed subsystem, not an afterthought.
- Some version of **Code Mode** (cf. Cloudflare): let the model write code that calls tools,
  instead of round-tripping every call through the context window.

### R8 — Observability & accounting
- Easy tracking of **token usage, cache hit/miss, cost/billing**, per agent / run / tool / provider.
- Good logging practice throughout; easy to debug.

### R9 — Error handling with intent
- Tool errors must be **throwable back to the model** to recover from.
- Errors can carry **custom messages containing further instruction** for the model,
  distinct from operator-facing/user-facing error text.

### R10 — Security first, audit-ready
- Early, structural focus on security.
- Harnesses built on Frey should *inherit* properties that make a **security audit** go well:
  sandboxing, capability/permission model, secret handling, audit log, supply-chain hygiene.

### R11 — Skills
- **Skills** and **discoverable skills** are part of the core model, not a bolt-on.

### R12 — Multi-modality
- Images, audio, documents, etc. across providers, with graceful degradation.

### R13 — Agent-authorable
- **Coding agents must be able to pick up Frey and perform.** Docs, naming, error messages,
  and types should be legible to an LLM with no prior exposure. This is a design constraint,
  not a docs task.

### R14 — Engineering quality
- Modular, debuggable, idiomatic Rust; use Rust's strengths (types, ownership, async,
  zero-cost abstraction, single-binary deploy) to be *structurally* better, not just faster.
- Documentation once code exists.

## Positioning requirement

There must be a **clear, statable reason** to switch from Rig / AutoAgents / OpenFANG / etc.
"It's Rust and it's new" is not a reason. Find the wedge; verify it isn't already occupied.

## House rules for this project

- Diagrams in notes/docs: **Mermaid**, never ASCII art.
- Public GitHub is the destination → commit hygiene, no co-author trailers, license chosen early.
- Verify claims (esp. "nobody has done X") with adversarial re-search before asserting.
- Research notes live in `notes/research/`, decisions in `notes/adr/`.

## Open questions to resolve during research

1. What do Rig/pydantic-ai users actually complain about? (wedge candidates)
2. What is the *current* MCP spec revision and what does "stateless server" mean precisely?
3. Does an official Rust MCP SDK exist and is it good enough to build on?
4. What is the state of the art in tool-count scaling (tool search, RAG-over-tools, code mode)?
5. What exactly do the Claude Agent SDK / Codex SDK expose that we can adapt?
6. Per-provider caching mechanics: explicit vs automatic, TTLs, pricing, breakpoints.
7. Sandboxing on Linux/macOS/Windows: what is actually achievable from Rust today?
8. Skills: is there a spec to conform to, or do we define ours?
