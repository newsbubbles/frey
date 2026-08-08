# Frey — planning notes

A Rust-native agent framework and harness toolkit. This directory is the planning record;
no code exists yet, by design.

## Read in this order

| File | What it is |
|---|---|
| [00-seed-requirements.md](00-seed-requirements.md) | The founding brief, turned into numbered requirements R1–R14. Source of truth for intent. |
| [research/01-protocol-and-platform-layer.md](research/01-protocol-and-platform-layer.md) | MCP `2026-07-28` (the stateless rewrite), `rmcp`, Anthropic tool search + programmatic tool calling, Cloudflare Code Mode, Agent Skills. |
| [research/02-provider-nuance-matrix.md](research/02-provider-nuance-matrix.md) | Anthropic / OpenAI / OpenRouter mechanics: caching, usage fields, item models, reasoning round-trip, subscription-auth ToS reality. |
| [research/03-landscape-and-wedge.md](research/03-landscape-and-wedge.md) | Rig, AutoAgents, OpenFANG, ADK-Rust, pydantic-ai. The wedge, and five attempts to kill it. |
| [research/04-security-and-sandboxing.md](research/04-security-and-sandboxing.md) | Threat model, 2026 injection-defense state of the art, per-platform sandboxing, the secure shell tool, taint as types. |
| [research/05-rust-building-blocks.md](research/05-rust-building-blocks.md) | Async traits, `tower`, `schemars`, OTel, determinism, crate hygiene, runtime hazards. |
| [architecture/00-overview.md](architecture/00-overview.md) | The claim, the principles, the crate graph, and how a run actually flows. |
| [architecture/01-core-api.md](architecture/01-core-api.md) | Concrete type and trait sketches: items, usage, cache plan, taint, capabilities, errors, tools, providers, events. |
| [architecture/02-protocols.md](architecture/02-protocols.md) | MCP client/server + legacy shim, A2A v1.0 both sides, AG-UI, and the one `NeedsInput` type all three share. |
| [architecture/03-context-engine.md](architecture/03-context-engine.md) | Budget, presentation, discovery, skills ladder, cache planner and its warning catalogue, code mode. |
| [architecture/04-security.md](architecture/04-security.md) | Capability model, Rule of Two, taint as types, the four sandbox backends, the secure shell, injection posture. |
| [architecture/05-multi-agent.md](architecture/05-multi-agent.md) | Sub-agents vs delegated vs peer, context inheritance, four orchestration primitives, streaming through the tree. |
| [architecture/06-harness.md](architecture/06-harness.md) | `Harness`, surfaces, sessions as journals, approvals, `frey-cli`, and `frey doctor`. |
| [architecture/07-testing.md](architecture/07-testing.md) | Six test tiers, the claim→falsifying-test table, `frey-testkit`, cassettes, red-team corpus, CI matrix. |
| [architecture/08-config.md](architecture/08-config.md) | `frey.toml` worked example, layering, validation rules, published JSON Schema. |
| [adr/decision-log.md](adr/decision-log.md) | ADR-0001 … ADR-0018, plus verification results and what is still open. |

## The claim, in one sentence

> Frey is the Rust agent framework where the context window is a managed resource — with a budget,
> a cache plan, and a provenance label — and where tools, skills, and code-mode are three
> presentations of one progressively-disclosed capability catalog.

## House rules

- Diagrams are **Mermaid**, never ASCII.
- Every research claim carries a source link and is re-verified before it becomes code.
- "Nobody has done X" claims get an adversarial re-search that tries to kill them first.
- Notes are written to be read by a coding agent as much as by a human.
