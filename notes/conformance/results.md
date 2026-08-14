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

## The headline

**0 of 6 servers that answered speak the `2026-07-28` stateless revision.**
