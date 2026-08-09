# Tools

## Two traits, and which to implement

- **`Toolset`** — a collection. Async, fallible, and asked for its definitions *once per step*
  rather than once at startup, because what should be visible depends on the task, the remaining
  budget, and the current policy. This is what an MCP server exposes.
- **`ToolHost`** — what the agent loop calls. Simpler and synchronous.

Implement `Toolset` if there is any chance the tools will also be served over MCP, shared between
agents, or filtered by relevance. Implement `ToolHost` for a fixed handful.

> **Known rough edge.** Bridging `Toolset` to `ToolHost` currently means writing an adapter, and
> because `ToolHost::definitions` is synchronous and infallible, that adapter ends in
> `unwrap_or_default()` — so a toolset that *fails* to list its tools presents none instead. This is
> a real quiet-degradation bug caused by a trait signature, it is the top-ranked item in
> [`notes/dogfood/01-demo-projects.md`](../notes/dogfood/01-demo-projects.md), and fixing it is a
> breaking change intended before 0.2.

## The macro

```rust
use frey::tool;

/// Read a file from the workspace.
#[frey::tool]
async fn fs_read(
    /// Path relative to the workspace root.
    path: String,
) -> Result<String, ToolError> {
    std::fs::read_to_string(&path).map_err(|e| {
        tool_err!(NotFound, "no file at {path}")
            .guide("List the directory with `fs_list` first.")
            .suggest(["fs_list"])
    })
}
```

**Parameter doc comments become schema descriptions, and a tool with no description does not
compile.** That is not style enforcement. Tool search indexes four fields — name, description,
argument names, argument descriptions — so an undocumented parameter is lost search surface, and a
tool becomes measurably harder to find once your catalog outgrows the context window. Writing the
doc comment and making the tool discoverable are the same act, which is the only way the habit
survives a deadline.

## Errors the model can act on

This is the part worth getting right. `ToolError` is typed by *audience*:

| Field | Goes to | Contains |
|---|---|---|
| `model()` | the context window | a summary, guidance, suggested tools, a schema hint |
| `operator()` | your logs | the diagnostic detail, stack traces, hostnames |
| `user()` | a UI, if any | something a person should read |

A test asserts operator diagnostics can never reach the context window, and another that the AG-UI
projection never sends a stack trace to a browser.

**Guidance is what makes a failure recoverable.** Compare:

```rust
// The model retries with identical arguments, forever.
ToolError::new(ToolErrorKind::NotFound, "not found")

// The model does something different next turn.
ToolError::new(ToolErrorKind::NotFound, "there is no memory with id `m404`")
    .guide("Call recall to find the right id, or remember the thing first.")
    .suggest(["recall", "remember"])
```

That difference is the single largest lever on whether a weak model is usable or merely slow.

## Outcomes

`ToolOutcome` has four variants and they are not interchangeable:

- **`Ok`** — success. The value is `Untrusted` by construction; you do not choose the label.
- **`Failed`** — it went wrong. The model sees it and may retry.
- **`Denied`** — policy refused. Distinct from `Failed` so the model can tell "try again differently"
  from "you may not do this".
- **`NeedsInput`** — cannot proceed without something from outside. See [MCP](mcp.md#approvals).

## Arguments are validated for you

Frey checks arguments against the declared schema before your tool runs — `type`, `required`,
`properties`, `enum`, `additionalProperties` — on **every** dispatch surface: the agent loop, the
MCP server, and multi-agent spawn.

This is a deliberately small subset rather than full JSON Schema, because that is the actual shape of
tool argument schemas and a complete validator would mean a heavyweight dependency in the hot path
of every call.

Two behaviours that exist because models really do this:

- `3.0` satisfies an `integer`. JSON has one number type and rejecting it costs a round trip for
  nothing.
- An **optional** argument explicitly set to `null` is treated as absent, which is how models say
  "not applicable". A **required** one set to `null` is an error, because that is how they say
  nothing at all.

## Untrusted by construction

Every tool result is `Untrusted<ToolContent>`. You cannot forget the label because you never apply
it — Frey does, at the boundary, with the provenance recording which tool and which server.

This matters most where it is least obvious. A memory store is the ideal place to park an injected
instruction: written once, retrieved much later, and by then it looks like the agent's own
recollection rather than like input.

## Risk and approvals

```rust
ToolDefinition::new(…).with_risk(Risk::High)
```

**Risk comes from the declaration, never from the model or the tool's own account of itself.** An
approval prompt shows the *literal action* — the exact command, URL, or statement — never a
natural-language summary, because a summary is precisely where an instruction injected upstream
survives review by the person clicking yes.

## Large catalogs

When you have more tools than context:

```rust
let results = Bm25Search::new(&definitions).search("postgres dialect", 5);
```

`RegexSearch` mirrors the provider-native semantics deliberately, down to the 200-character pattern
limit and the 5-result default, so a query behaves the same whether Frey ran it or delegated it to
the provider.

Both index the same four fields the providers do — which is the concrete reason an undocumented
parameter is a defect rather than a style preference.

## See also

- [thicket's toolset](https://github.com/newsbubbles/thicket/blob/main/src/toolset.rs) — five tools,
  real error guidance, structured output.
- [switchboard's toolset](https://github.com/newsbubbles/switchboard/blob/main/src/toolset.rs) —
  approval gates over the multi round-trip pattern.
