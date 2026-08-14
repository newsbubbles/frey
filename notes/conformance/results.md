# MCP conformance sweep

Frey's own MCP client against servers Frey did not write. No inference; this costs nothing
to run. Every claim in `docs/mcp.md` previously rested on `FakeToolset` and on Frey's
server answering Frey's client, which is a test of the code and not of the protocol.

`churns` is the column worth reading: Frey re-sorts listings defensively because a server
can rewrite a cached prompt prefix, and until this sweep that defence had never met a
server that might.

| server | reached | stateless | tools | thin descriptions | bad schemas | churns |
|---|---|---|---|---|---|---|
| memory | yes | handshake | 9 | 0 | 0 | no |

Unreachable rows mean the server would not start on this machine — usually a missing
`npx` or `uvx` — and are **not** passes.

## The headline

**0 of 1 servers that answered speak the `2026-07-28` stateless revision.**
