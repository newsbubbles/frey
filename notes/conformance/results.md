# MCP conformance sweep

Frey's own MCP client against servers Frey did not write. No inference; this costs nothing
to run. Every claim in `docs/mcp.md` previously rested on `FakeToolset` and on Frey's
server answering Frey's client, which is a test of the code and not of the protocol.

`churns` is the column worth reading: Frey re-sorts listings defensively because a server
can rewrite a cached prompt prefix, and until this sweep that defence had never met a
server that might.

| server | reached | stateless | tools | thin descriptions | bad schemas | churns |
|---|---|---|---|---|---|---|
| filesystem | yes | handshake | 14 | 0 | 0 | no |
| memory | yes | handshake | 9 | 0 | 0 | no |
| sequential-thinking | yes | handshake | 1 | 0 | 0 | no |
| everything | yes | handshake | 12 | 0 | 0 | **yes** |
| context7 | yes | handshake | 2 | 0 | 0 | no |
| chrome-devtools | yes | handshake | 29 | 0 | 0 | no |
| git | no | — | — | — | — | — |
| fetch | no | — | — | — | — | — |
| time | no | — | — | — | — | — |
| sqlite | no | — | — | — | — | — |

Unreachable rows mean the server would not start on this machine — usually a missing
`npx` or `uvx` — and are **not** passes.

- `git`: stateless: AttributeError: 'Server' object has no attribute 'list_tools'; handshake: AttributeError: 'Server' object has no attribute 'list_tools'

- `fetch`: stateless: ImportError: cannot import name 'McpError' from 'mcp.shared.exceptions' (C:\Users\dumbass\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\uv\cache\…; handshake: ImportError: cannot import name 'McpError' from 'mcp.shared.exceptions' (C:\Users\dumbass\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\uv\cache\…

- `time`: stateless: ImportError: cannot import name 'McpError' from 'mcp.shared.exceptions' (C:\Users\dumbass\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\uv\cache\…; handshake: ImportError: cannot import name 'McpError' from 'mcp.shared.exceptions' (C:\Users\dumbass\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\uv\cache\…

- `sqlite`: stateless: AttributeError: 'Server' object has no attribute 'list_resources'; handshake: AttributeError: 'Server' object has no attribute 'list_resources'

## What churned

- **`everything`** — a different set of tools between two identical calls — gained `simulate-research-query` (first listing had 12, second had 13)

## Where MCP startup time goes

Milliseconds, cold, one sample each — indicative, not a benchmark.

`spawn` is `Command::spawn` returning. **`to first byte`** is everything before the server says anything: interpreter startup, `npx`/`uvx` package resolution, module imports — plus the first round trip, since the first line read *is* the answer to `server/discover`. `list` is the `tools/list` round trip after that.

`negotiate` reads 0 for every row, and that is a property of these servers rather than a fast path: none of them answers `server/discover`, so there is never a successful negotiation to time and its cost is already inside `to first byte`.

| server | spawn | to first byte | negotiate | list | total |
|---|---|---|---|---|---|
| filesystem | 28 | **4496** | 0 | 14 | 4510 |
| memory | 20 | **3168** | 0 | 11 | 3180 |
| sequential-thinking | 21 | **3794** | 0 | 10 | 3804 |
| everything | 24 | **6567** | 0 | 39 | 6606 |
| context7 | 33 | **6808** | 0 | 43 | 6851 |
| chrome-devtools | 24 | **8907** | 0 | 33 | 8940 |

**33740 ms of this happens before any server says a word; 150 ms is protocol.** The first number belongs to somebody else's process starting up and is the same for every framework in every language. The second is the only part a client's design controls.

The conclusion is unwelcome if you were hoping the stateless revision is a speed feature. It is not: skipping a handshake saves one round trip out of the 150 ms that every round trip here costs put together. **Statelessness is a scaling property — any replica can serve any request, with no session affinity — and selling it as latency would be selling a rounding error.**

## The headline

**0 of 6 servers that answered speak the `2026-07-28` stateless revision.**
