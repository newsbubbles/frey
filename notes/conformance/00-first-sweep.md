# The first sweep: what happens when the client meets a server it did not write

Frey's MCP client had never connected to a server Frey did not write. Every claim in `docs/mcp.md`
rested on `FakeToolset` and on Frey's own server answering Frey's own client over a loopback — a
real test of the code and no test at all of the *protocol*, because a client and a server written by
one person on one afternoon agree about everything, including their shared misreadings.

`cargo xtask conformance` fixes that. Ten stdio servers, no inference, no cost. The generated table
is in [`results.md`](results.md); this is what it means.

## Two findings

### 1. Nothing speaks the revision Frey is built on

**Six of six servers that answered needed the legacy `initialize` handshake.** Not one answered
`server/discover`.

Frey is built on MCP `2026-07-28` — no handshake, no `Mcp-Session-Id`, no SSE resumability — and
ships a shim for older servers. That shim is not a compatibility nicety. **It is the only code path
that has ever worked against a real server**, and it had been described in the documentation as the
fallback.

This is not a defect in Frey and it is not a defect in the servers: the revision is recent and the
ecosystem has not moved. What it changes is which claim the repository is entitled to make. *Frey
speaks the stateless revision* is settled by frey's own client and server agreeing. *Frey works with
MCP servers* is settled by the shim, and the shim is now the tested path rather than the assumed
one.

The number to watch over the next six months is that column going from 0 to non-zero. It is the
cheapest possible measurement of whether the revision Frey bet on is the one the ecosystem adopts,
and re-running the sweep costs nothing.

### 2. A reference server changes its tool list between two identical calls

`@modelcontextprotocol/server-everything`, listed twice in a row, with nothing in between:

```
first : echo, get-annotated-message, get-env, get-resource-links, get-resource-reference,
        get-structured-content, get-sum, get-tiny-image, gzip-file-as-resource,
        toggle-simulated-logging, toggle-subscriber-updates, trigger-long-running-operation
second: … the same twelve, plus simulate-research-query
```

**A tool appears.** Same server, same process lifetime, same request.

This is the failure Frey's defensive re-sorting exists for, met for the first time — and it is worth
being precise, because the defence does not actually cover it.

`client.rs:181` sorts the listing by name and dedupes, and the test that pins it is called
`a_server_that_reorders_its_listing_cannot_churn_the_tool_block`. That is exactly right for
*reordering*, which is what the doc comment anticipated. It does nothing for *membership*: sorting a
list that gained an element still yields a different list, the tool block still rehashes, and the
cached prefix is still rewritten.

So the honest statement is:

| A server that… | Frey's defence |
|---|---|
| reorders its listing | **prevented** — re-sorted before hashing |
| returns duplicate names | **prevented** — deduped |
| adds or removes a tool | **reported, not prevented** — `Warning::CacheChurn` on the tool block |
| changes a description | **reported, not prevented** — same |

Reporting is the correct answer for the last two. A tool that genuinely appeared should appear; the
alternative is a client that hides a server's capabilities to protect a cache. What matters is that
the cost is *visible*, and until two days ago it was not: churn detection sat behind an early return
that fired whenever the breakpoint budget was zero, so on every automatic-caching provider — which
is two of Frey's three dialects, including the only one in production use — `CacheChurn` could not
fire at all. A real server doing this to a real cache would have shown up only on the invoice.

The advice string the warning carries was written before any of this and turns out to be exactly
right: *"the tool block changed between turns. A toolset that reorders its listing, or a description
containing a timestamp or counter, will do this."*

## What the sweep does not tell you

- **Six servers is not the ecosystem.** It is the servers a person reaches for first, which is a
  different and also useful sample.
- **`tools/list` is not `tools/call`.** Nothing here exercises a tool. The next sweep should, on the
  servers where calling something is free and side-effect-free.
- **Four Python servers never started**, and their failures are upstream — `mcp-server-fetch` and
  `mcp-server-time` fail with `ImportError: cannot import name 'McpError'`, `mcp-server-git` and
  `mcp-server-sqlite` with `AttributeError: 'Server' object has no attribute 'list_tools'`. The
  published servers are incompatible with the current release of the SDK they depend on. Their rows
  are kept in the table because deleting them would hide an ecosystem finding, and an unreachable
  row is explicitly **not** a pass.
- **Descriptions and schemas were uniformly fine.** Zero thin descriptions and zero unusable schemas
  across 67 tools. Frey's discoverability check has nothing to complain about here, which is worth
  saying plainly rather than leaving as an absence.

## Disclosure

The `everything` finding is a bug report and it goes to the maintainers before it goes anywhere
public. It is a *demonstration* server whose purpose is to exercise every protocol feature, so a
tool that appears on the second listing may well be deliberate — which would make it a documentation
gap rather than a defect, and either way theirs to say.
