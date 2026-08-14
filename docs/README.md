# Frey documentation

Start here.

| Page | Read it when |
|---|---|
| [Why Frey](why-frey.md) | You are comparing this against pydantic-ai, Rig, LangGraph or the AI SDK. |
| [Getting started](getting-started.md) | You want a working agent in ten minutes. |
| [Providers](providers.md) | You need to pick one, or make Frey talk to something it has never heard of. |
| [Tools](tools.md) | You are writing the things your agent can do. |
| [MCP](mcp.md) | You want to call an MCP server, or be one. |
| [Context and caching](context-and-caching.md) | Your bill is larger than you expected. |
| [Security model](security-model.md) | Someone is going to audit this. |
| [Testing an agent](testing.md) | You want tests that do not cost money or flake. |
| [FAQ](faq.md) | You are deciding whether to use this at all. |

## Beyond the docs

- **[`notes/`](../notes/README.md)** — the design record. Research, architecture with Mermaid
  diagrams, twenty ADRs with the reasoning behind each decision, and
  [`PROGRESS.md`](../notes/PROGRESS.md), which records what each milestone found including the bugs
  and the scope reductions.
- **[`notes/dogfood/`](../notes/dogfood/)** — what happened when the framework was first used for
  real, including the parts that went badly.
- **API documentation** — `cargo doc --open --all-features`. The doc comments carry the *reasoning*,
  not just the signatures; most of what is interesting about this codebase is in them.

## Worked examples

Three complete projects, each with a `FINDINGS.md` about what was awkward:

- **[thicket](https://github.com/newsbubbles/thicket)** — graph-shaped agent memory over MCP. Read
  this for tools and the agent loop.
- **[switchboard](https://github.com/newsbubbles/switchboard)** — a hosted, stateless MCP server
  with approval gates. Read this for the protocol.
- **[abacus](https://github.com/newsbubbles/abacus)** — tool calling measured against code mode.
  Read this for the result that changed what the docs say about code mode.

And two runnable in-repo examples that need no API key:

```bash
cargo run -p frey --example cache_planning
```

```bash
cargo run -p frey --example agent_loop
```
