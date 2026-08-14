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
the documentation said it ships a shim for older servers.

> **Correction, same day.** It does not. `negotiate()` identifies a pre-stateless server and writes
> `stateless: false` into `ServerIdentity`, and **nothing reads that field**; the client never sends
> `initialize`. So the sentence that stood here — *"the shim is the only code path that has ever
> worked against a real server"* — was wrong twice over: the shim does not exist, and no Frey code
> path has ever run against a real server, because `McpClient` ships no `Transport` outside
> `#[cfg(test)]` and this sweep hand-rolls its own JSON-RPC. Recorded as
> [I-011](../INCIDENTS.md). What follows is a measurement of the ecosystem, which is what it always
> was.

The ecosystem not having moved is not a defect in Frey and not a defect in the servers; the revision
is recent. What it changes is which claim the repository is entitled to make. *Frey speaks the
stateless revision* is settled by frey's own client and server agreeing. ***Frey works with MCP
servers* is not settled by anything** — it needs a shipped transport and the handshake, and until
both exist the honest reading of this table is that it describes the servers and says nothing about
the client.

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

**A tool appears.** Same package, same arguments, same request — but, and this matters for anyone
reporting it upstream, **two separate processes**: the sweep spawns, lists, and kills, twice. So this
is not a server mutating its catalog while running; it is two runs of one server disagreeing, which
is a weaker and stranger finding. A single-process double-list would settle which. Either way the
consequence for a cached prompt is identical, because a client reconnecting is exactly what happens
between agent runs.

**It reproduced.** The sweep was run twice over, an hour apart, for an unrelated reason — a fix to
the table renderer — and both full runs report the same twelve-then-thirteen with the same tool
appearing. That moves it from something seen once to something that happens, which is the difference
between a note and a bug report.

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

## 3. Startup is not a protocol problem, and "stateless is faster" is not true

Added 2026-08-14, from the question *"MCP initialization takes a few seconds — is Frey better?"*

The sweep now times each server from `Command::spawn` to a usable catalog. Two full runs about an
hour apart:

| | before any server says a word | protocol round trips |
|---|---|---|
| run A | 73,090 ms | 102 ms |
| run B | 33,740 ms | 150 ms |

**The absolute numbers are noise — they moved by more than 2× between two runs of identical code on
one machine**, because `npx -y` package resolution and interpreter startup depend on what is warm.
Anything quoting a single figure here is quoting the state of a cache. The *ratio* is not noise: in
both runs, **more than 99.5% of the time to a tool catalog was spent before the server answered
anything.**

That part belongs to somebody else's process starting up. It is identical for Frey, for Rig, for
pydantic-ai, and for a shell script. So:

- **The stateless revision is not a latency feature.** Skipping the `initialize` handshake saves one
  round trip out of a protocol total of roughly 100–150 ms across six servers — against tens of
  seconds of process startup. Selling it as speed would be selling a rounding error.
- **What it is instead is a scaling property**: any replica can serve any request, with no session
  affinity. That is the claim `switchboard` demonstrates and the one worth making.
- **The real MCP performance question is not "how fast is your handshake"** but "how often do you
  pay 4–9 seconds to start the process at all". Frey has no server reuse or connection pooling for
  MCP, so it currently has no answer to that either. Recorded rather than fixed.

## What the sweep does not tell you

- **Six servers is not the ecosystem.** It is the servers a person reaches for first, which is a
  different and also useful sample.
- **`tools/list` is not `tools/call`.** Nothing here exercises a tool. The next sweep should, on the
  servers where calling something is free and side-effect-free.
- **Four Python servers never started**, and their failures are upstream — `mcp-server-fetch` and
  `mcp-server-time` fail with `ImportError: cannot import name 'McpError'`, `mcp-server-git` with
  `AttributeError: 'Server' object has no attribute 'list_tools'`, and `mcp-server-sqlite` with
  `list_resources`. The published servers are incompatible with the current release of the SDK they
  depend on. Their rows are kept in the table because deleting them would hide an ecosystem
  finding, and an unreachable row is explicitly **not** a pass.
- **Descriptions and schemas were uniformly fine.** Zero thin descriptions and zero unusable schemas
  across 67 tools. Frey's discoverability check has nothing to complain about here, which is worth
  saying plainly rather than leaving as an absence.

## Disclosure

The `everything` finding is a bug report and it goes to the maintainers before it goes anywhere
public. It is a *demonstration* server whose purpose is to exercise every protocol feature, so a
tool that appears on the second listing may well be deliberate — which would make it a documentation
gap rather than a defect, and either way theirs to say.
