# Frey

**A Rust agent framework where the context window is a managed resource.**

> ⚠️ **Pre-alpha.** `frey-core` exists; nothing else does yet. The design is written down in
> [`notes/`](notes/README.md) and is worth reading before the code.

Frey treats the context window as what it actually is — a scarce, cache-sensitive, ordered
resource — and treats tools, skills, and code-mode as three presentations of one
progressively-disclosed capability catalog.

## Why another one

The Rust agent ecosystem in 2026 is [crowded at the framework level and thin on
infrastructure](notes/research/03-landscape-and-wedge.md). Frey is aimed at the thin part:

- **Context economy.** A cache planner that knows every provider's rules — Anthropic's four
  breakpoints and per-model minimum prefix, OpenAI's `prompt_cache_key` sharding, OpenRouter's
  per-provider explicit-vs-automatic split — and that *refuses to place a breakpoint on a segment
  that changed last turn*, telling you which one and what it costs.
- **Security you can hand to an auditor.** Capability grants, a cross-platform sandbox that fails
  closed, and information-flow labels carried in the type system so that "where does untrusted
  data become trusted" is answered by `grep endorse` plus a runtime log.
- **Harness-grade runtime.** Deterministic replay from an event-sourced journal, OpenTelemetry
  across sub-agent and MCP boundaries, AG-UI without an adapter, and delegation to `claude` or
  `codex` as sub-agents.

Built on MCP `2026-07-28` — the revision that removed protocol sessions — with a shim for older
servers. A2A v1.0 and AG-UI are first-class, because
[all three protocols converged on the same interrupt concept](notes/architecture/02-protocols.md).

## What works today

```rust
use frey_core::prelude::*;
use frey_core::taint::Tainted;

// A tool returns plain data. The framework labels it at the boundary.
let page: Tainted<String> = Tainted::from_tool("http_get", body);

// Acting on it requires something trusted to vouch for it — and a parser is the honest
// voucher, because narrowing the type *is* the check.
let url = page.validate::<OnEgressAllowlist>()?;   // audited, with a call-site record
```

Passing untrusted data to a sink that requires trusted input is a **compile** error, not a runtime
check. See [`crates/frey-core/tests/taint_ergonomics.rs`](crates/frey-core/tests/taint_ergonomics.rs).

## Status

| Crate | State |
|---|---|
| `frey-core` | taint lattice, audit trail, error model — 28 tests |
| everything else | designed, not written — see [`notes/architecture/`](notes/architecture/) |

## Development

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Regenerate the compile-fail expectations after a compiler upgrade:

```bash
TRYBUILD=overwrite cargo test --test ui
```

## Licence

MIT OR Apache-2.0, at your option.
