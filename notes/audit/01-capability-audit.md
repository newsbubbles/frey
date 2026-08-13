# Capability audit — 2026-08-11

Against `main` at `63c717d`. 460 tests, clippy clean under `-D warnings`, every feature combination
building alone.

## Why this audit, and how it was run

Seven bugs in two days shared one shape: **a declared capability or a documented affordance with no
producer on the path that mattered.** The type existed. The documentation existed. Consumer-side
tests existed. The emit site did not.

So the method here is not "does the code work" — it does, and it is well tested. It is: *for every
capability this project claims, find the thing that produces it, and find the path that reaches it.*

Every capability lands in one of three states:

| State | Meaning |
|---|---|
| **Wired** | A producer exists on a runtime path — the agent loop, the MCP server, or the CLI. Using Frey normally exercises it. |
| **Library** | The code works and is tested, but nothing in Frey calls it. **You must call it yourself.** |
| **Declared only** | A type, trait, variant or doc claim with no implementation anywhere. |

The distinction between the first two is the whole point. A `Library` capability is not broken and
not a bug — but a reader of the README will believe it participates, and it does not.

---

## The headline

**Frey is a well-built library of agent parts, plus one loop that uses about a third of them.**

The agent loop imports fourteen things. The facade re-exports roughly forty. The gap is not dead
code — most of it is good code with real tests — but it is code the framework never calls on your
behalf, and several pieces of it are described in the README as though it does.

Two claims are currently wrong rather than merely optimistic, and both are fixed alongside this note:

1. **There is no shell tool, and no tools at all.** R5 asked for "real toolsets in the box including
   an actually secure shell tool."
2. **Nothing is ever confined.** `frey-sandbox` has no enforcement code and `SandboxBackend` has no
   implementation.

---

## Wired — exercised by using Frey normally

| Capability | Producer | Evidence |
|---|---|---|
| Context budgeting | `Agent::run` | `budget::{Budgeter, ContextBudget}` imported by the loop |
| **Cache planning** | `Agent::run` | `CachePlanner::plan` every turn, `check_lookback` per response |
| Segment hashing | `Agent::run` | `hash_parts` over tool block and turns |
| Provider dispatch | `Agent::run` | `ModelProvider::complete` |
| Provider capabilities | all three dialects | `profiles` is the most-referenced module in the tree |
| Wire mapping | `HttpProvider` | Anthropic / OpenAI Responses / OpenRouter + config-defined |
| SSE decoding | `HttpProvider::stream` | keepalives, chunk boundaries, CRLF |
| Argument validation | loop **and** MCP server | `check_arguments` before every dispatch |
| Caller policy | `Agent::run` | `PolicyLayer::check` before every dispatch |
| Typed errors | loop + MCP server | `ToolError` audience split reaches both surfaces |
| Journal + replay | `Agent::run` | every non-deterministic effect, round-trips whole |
| Usage + cost | `Agent::run` | `UsageTotals`, cost `Reported` not estimated |
| Warnings | `Agent::run` | 7 variants emitted; `Display` added |
| Transcript events | `Agent::run` | 11 of 12 `EventKind` variants emitted |
| Taint labelling | everywhere | applied at every boundary by construction |
| MCP client | user code | tested against a fake transport |
| **MCP server** | user code | thicket and switchboard both run on it |
| `doctor` | `frey-cli` | JSON output pinned as an API |

This list is the honest product. It is also the part that has been driven hard: deadnet is running
thousands of live sessions through the first fourteen rows.

---

## Library — works, tested, and nothing calls it

Each of these is reachable only if **you** write the call. Verified by grepping every reference from
outside its own crate: in each case the only hits are prelude re-exports.

| Capability | State | What it means for a caller |
|---|---|---|
| `skills` | parse + index only | Skill selection is yours. The ladder is not in the loop. |
| `codemode` | `generate_api`, `bindings`, `strategy_for` | No execution path. See `notes/dogfood/01` — delegation is the only working mode. |
| `search` | `Bm25Search`, `RegexSearch` | **Tool search is not in the loop.** `Item::Discovery` and `EventKind::Discovered` have zero producers — the same hole from the other end. |
| `ToolRegistry` | complete | The loop takes a `ToolHost`, not a registry. |
| `ApprovalLayer` | complete | Only `PolicyLayer` is in the loop. |
| `RedactLayer` | complete | Not in the loop. Nothing redacts unless you do. |
| `TruncateLayer` | complete | Not in the loop. The loop reports `bytes_elided` a tool sets itself. |
| `builtin` validators | complete | `InWorkspace`, `AllowedProgram`, `OnEgressAllowlist`, `ParsedJson`, `ShellArgv`. Ingredients, not tools. |
| `multi::spawn` | complete | A capability *check* returning a `Child` descriptor. It spawns nothing. |
| `a2a` | complete | Types and lifecycle. No server, no client, no transport. |
| `agui::project` | complete | A serialiser. Nothing streams to a frontend. |
| `harness::session` | complete | Not used by the loop. |
| `AgentProvider` / `AgentCli` | complete | No loop delegates to it. Wire format tested against recorded output; **never run against a live vendor binary.** |
| `ModelProvider::stream` | complete | `Agent::run` only calls `complete`. |
| `Tool` trait | two impls, both tests | Nothing consumes it. `Toolset` and `ToolHost` are the live abstractions. |
| `#[frey::tool]` | generates a struct | It does **not** implement `Tool`, `Toolset` or `ToolHost`. You still write the adapter. |

---

## Declared only — no implementation anywhere

Ranked by consequence.

### A1 — There is no shell tool. There are no tools at all.

R5: *"There should also be some existing toolsets including an actually secure shell tool."*

`frey-tools::builtin` contains five **validators** and two helpers. No tool. Nothing in the workspace
spawns a process for a tool — the only `Command::new` is the agent-CLI delegation adapter.

This is defensible as a design (a framework that ships no tools ships no CVEs) but it is not what the
seed requirement asked for, and the README's crate table reads as though `frey-tools` contains tools.
It contains the layers a tool passes through and the validators you'd build one with.

### A2 — Nothing is ever confined.

`SandboxBackend` is a trait with one implementation, a `#[cfg(test)] Stub`. `frey-sandbox` is two
pure modules:

- `policy.rs` — `validate`, `decide`, `allow_degraded`. Decides whether an exec *would* be allowed.
- `probe.rs` — `linux_availability(abi, lsm_enabled)`, `macos_availability()`, … These take the ABI
  level and the `lsm=` flag **as parameters**. They do not detect anything; they report what would be
  true given values someone else supplies.

There are no syscalls. No Landlock ruleset is ever applied, no Seatbelt profile compiled, no
AppContainer created. Which is consistent, because nothing executes anything (A1) — but the README
says *"Cross-platform confinement that fails closed"* and `docs/security-model.md` presents a table
of platform mechanisms as if they are implemented. **That doc is mine, written two days ago, and it
is the most overclaiming page in the repo.** Corrected alongside this audit.

The honest description: Frey ships a sandbox **policy language and a capability model**, plus a
degradation-reporting story. It does not ship a sandbox.

### A3 — Approvals cannot happen inside the agent loop.

`RunError::NeedsInput` exists and has **zero producers**. A tool that returns
`ToolOutcome::NeedsInput` inside `Agent::run` falls through `render_outcome`'s catch-all and is
rendered to the model as an error string: *"This action needs approval that was not available."*

That is not silent, which is why it has gone unnoticed. But it means human-in-the-loop approval —
the multi round-trip pattern, `InputRequest::Approval`, the literal-action rule — works **over MCP
and not in the loop**. switchboard proved the server path end to end; there is no equivalent path
for an in-process agent.

This matters more now than it did last week, because `ToolCx::resume` was added for the MCP server
and the loop has no equivalent.

### A4 — `ToolHost` is implemented once, in a test.

Nothing in Frey implements the trait its own agent loop takes. Every application writes the same
adapter from `Toolset`, and because `ToolHost::definitions` is synchronous and infallible, every one
of those adapters ends in `unwrap_or_default()`.

Already the top-ranked item in `notes/dogfood/01-demo-projects.md` (D3) and unchanged: it is a
breaking change with a real design question inside it.

---

## Is it feature-complete?

**As a library: yes.** Every subsystem does what its documentation says, has tests that pass, and
those tests are mostly good ones. Nothing here is half-written.

**As a framework: no, and the gap is specific.** The thesis is *"tools, skills, and code-mode are
three presentations of one progressively-disclosed catalog."* Of that sentence:

- tools — presented, once per run, no disclosure
- skills — not in the loop
- code-mode — not in the loop, and cannot be without an engine
- progressive disclosure — `search` exists, has no producer, and its event and item variants have
  none either

The catalog machinery is real and the loop does not use it. That is one honest sentence and it
should be in the README instead of the current one.

**Nothing found in this audit is broken for deadnet.** They use the fourteen wired rows, all of which
are exercised by thousands of live sessions. The unwired parts are the ones they were told about in
`notes/dogfood/02-external-review.md`: skills, disclosure, approvals-in-loop.

---

## What to do, in order

1. **Correct the two wrong claims** — the sandbox and the tools. Done alongside this note.
2. **A3: make approvals reachable from the loop**, or say plainly that they are MCP-only. The
   machinery exists on both sides; what is missing is the handler hook on `Agent`.
3. **A4: `ToolHost::definitions`** — async and fallible, with the reduced-catalog decision made.
4. **Wire one disclosure path end to end** — `search` into the loop, producing `Item::Discovery` and
   `EventKind::Discovered`. Until one exists, the catalog thesis is a design and not a feature.
5. **Ship one real tool.** A single `fs_read` built from `InWorkspace` + the macro would prove the
   whole stack composes, and would be the first evidence that `#[frey::tool]` output can reach a
   loop without the user writing glue.

## A note on method, for next time

The reason this audit was possible in an afternoon is that the bug pattern gave it a query: *find the
producer.* Recommended as a standing check rather than a one-off — for every `pub` item, ask which
runtime path constructs it. Two of the strongest signals here cost one grep each:

- `EventKind::Discovered` — 0 constructions
- `RunError::NeedsInput` — 0 constructions

An enum variant no code ever builds is the cheapest possible detector for this class, and it found
two of the four significant gaps.
