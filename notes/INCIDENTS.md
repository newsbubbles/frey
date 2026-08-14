# Incidents

One entry per failure, with **`found_by`** on each: `system` (an alarm, a lint or a test fired),
`operator` (a person noticed something was off), or `code-reading` (found by looking, with nothing
having gone visibly wrong).

That field is the whole point of the file. A project with no incidents is indistinguishable from a
project with no instruments, and the ratio of `system` to `code-reading` is the only measurement
that separates them.

**The tripwire, from `notes/plan/EVIDENCE-PLAN.md` §7:** *if by day 21 there is no entry with
`found_by: system`, the instrument is dead rather than the system healthy.* The historical record
makes zero incidents in three weeks implausible — a 145-tool-call runaway found by reading an
invoice, a model going four-in-five to none within an hour on unchanged code, four provider errors
in a single night. Silence would mean the alarms cannot fire.

**Second tripwire, weekly:** two consecutive undetected fault drills and the work stops until the
alarms are fixed. See `internets/<world>/journal/DRILLS.jsonl`.

---

## 2026-08-14

### I-001 · The cache planner's headline warning was unreachable on the only dialect in use

**found_by: code-reading** · frey · severity: high

`CachePlanner::plan` returned before churn detection whenever the breakpoint budget was zero, and an
automatic-caching provider has a budget of zero by definition. So `Warning::CacheChurn` and
`Warning::BelowMinPrefix` were **structurally unreachable** on OpenRouter — the dialect deadnet runs
for every one of its sessions — while deadnet's own `ECONOMICS.md` names cache churn as the single
largest threat to its budget and says in terms that frey *"will warn rather than silently bleed"*.

Nothing had gone visibly wrong. Nothing could: the failure mode of undetected churn is a larger
invoice, and no invoice has ever been reconciled against an expectation.

**Fixed.** Churn is a property of the prompt rather than of who places the breakpoints; detection now
runs before the budget is consulted. The minimum-prefix check runs against the *leading* stable run,
which is what a longest-common-prefix cache can actually reuse.

**What would have caught it earlier:** the question nobody asked — *does what the planner produces
appear in the bytes we send?* That is now `frey_providers::marks::survey` and a test.

---

### I-002 · Nothing durable had ever been recorded

**found_by: code-reading** · deadnet · severity: high

Six worlds. `select count(*) from spend` returned **0 in all six**. `internets/*/journal/` held
**0 files in all six**. The table has the right columns, the directory is created on `world create`,
and `DESIGN.md` documents both. Nothing had ever called anything.

Thousands of live sessions, and the only record of any of them was a `println!` in a terminal that
had since been closed.

**Fixed.** `spend` is upserted per night; journals are written per person with the frey rev in the
filename; `MANIFEST.jsonl` carries what the STRICT schema cannot hold. See `deadnet/CLAUDE.md`.

---

### I-003 · An adapter declared a cache capability the API does not have

**found_by: system** · frey · severity: medium

The **first** `found_by: system` entry, and it arrived within minutes of the instrument existing.

`marks::survey` encodes one representative request through every dialect and counts the
`cache_control` markers that come out; a test asserts that a dialect handed breakpoints must emit
them. It went red on two of three on its first run:

- **Anthropic** honoured only `marks.last()`, collapsing a four-breakpoint Opus plan to one, dropping
  it entirely when the system prompt was empty, and never marking the tool block — three lines below
  a doc comment saying that it did.
- **OpenAI Responses** declared `explicit_available: true`. The Responses API has no breakpoint
  mechanism at all; `prompt_cache_key` is routing affinity. The planner therefore placed a mark on
  every request and the adapter discarded every one, silently, while the plan reported it as placed.

Neither was a new bug. Both had been true since the adapters were written, and both survived a
capability audit — because the audit asked *which code constructs this* and these two failures live
in the gap between a producer and a wire.

**Fixed.** `Request::mark_placement` resolves segment ids to wire positions once, for every adapter.

---

### I-004 · The disk filled and the compiler produced nonsense

**found_by: operator** · environment · severity: medium

`D:` reached 98% with ~10 GB free. Adding a second cargo target directory — the deadnet rebuild
airlock, which exists to make builds safe while nights run — pushed it over. `rustc` then emitted
`only metadata stub found for rlib dependency std`, `found invalid metadata files for crate core`,
and an internal compiler panic.

Diagnosed as a corrupted target directory and retried, twice, before checking `df`. **The error text
pointed at the toolchain and the cause was the environment**, which is the shape of failure that
costs the most time.

**Resolved** by removing seven unused `target/` directories from sibling projects, freeing ~5 GB,
plus a `cargo clean` of the affected crates. No data lost.

**Standing consequence:** this laptop is the host for the 30-night unattended run and it has ~20 GB
of headroom. Disk is now a known failure mode for that run rather than a surprise in the middle of
it — and it is exactly the class of thing the plan predicted when it argued for recording host
incidents rather than pre-buying a VPS to avoid them.

---

### I-005 · The upstream Python MCP reference servers do not start

**found_by: system** · ecosystem · severity: low

`cargo xtask conformance` connects frey's MCP client to third-party servers. Four of the **ten**
targets — all four Python ones, via `uvx` — fail before answering anything, and with **two different
errors**, which the first draft of this entry flattened into one:

```
mcp-server-fetch, mcp-server-time:  ImportError: cannot import name 'McpError'
                                    from 'mcp.shared.exceptions'
mcp-server-git:                     AttributeError: 'Server' object has no attribute 'list_tools'
mcp-server-sqlite:                  AttributeError: 'Server' object has no attribute 'list_resources'
```

Both shapes say the same thing: the published servers are incompatible with the current release of
the `mcp` Python SDK they depend on. Nothing to do with frey; recorded because the sweep's table
would otherwise show four blank rows and a reader would assume the client failed.

**Kept in the sweep deliberately.** "The upstream reference implementation is broken against the
current SDK" is a finding about the ecosystem, and deleting the row would hide it.

---

### I-006 · Two defects in the conformance sweep itself, on its first run

**found_by: system** · frey/xtask · severity: low

The instrument's own first outing found two things about the instrument:

1. **`npx` was reported as "program not found" on a machine with node on `PATH`.** On Windows `npx`
   is a `.cmd` shim and `Command::new` will not resolve it without the extension. All four npm
   targets read as unreachable, which is the same output an environment with no node at all would
   produce — a false negative wearing a true negative's clothes.
2. **The sweep hung.** One server never answered and there was no per-server timeout, so a check
   designed to run weekly and unattended blocked forever on its first execution.

**Fixed:** `.cmd` on Windows, a 90-second watchdog per target, and stderr captured so an unreachable
row says *why* rather than *no answer*. A sweep whose failures are opaque is half a sweep.

---

### I-007 · Fault drill · all five detected

**found_by: system (drill)** · deadnet · severity: none

First run of `deadnet drill`, against a copy of `the-log`:

| fault | stands for | detected | latency |
|---|---|---|---|
| silence | the scheduled task stopped, or the host rebooted into Windows Update | yes | 13 ms |
| cost-silence | the reverted `usage: {include: true}` regression | yes | 12 ms |
| dead-turns | `gpt-oss-120b` going four-in-five to none within an hour | yes | 10 ms |
| no-tools | a host answering with an empty catalog | yes | 14 ms |
| unclosed | a transcript that ends without a closing event | yes | 11 ms |

**Read this table narrowly, and it was overstated in its first draft.** Latency measures the
*detector*, not the pipeline: the drill writes manifest and journal state by hand and asks the
watcher to read it. That establishes the alarms fire on the state they are looking for. It does
**not** establish that a real failing night produces that state — for four of the five faults the
production path was traced by hand and does, and for `unclosed` it does not: `write_journal` is a
single terminal `fs::write` after the run returns, so a process that dies mid-run leaves no file at
all rather than a truncated one. That fault therefore stands for *a transcript that ends without a
closing event*, which is a real shape frey can produce, and not for the one the drill's own label
originally claimed.

End to end, detection is also bounded by how often `deadnet watch` runs, which is the number that
will matter once the nights are on a timer.

---

### I-008 · The watcher's first live reading is an alarm

**found_by: system** · deadnet · severity: expected

`deadnet watch internets/the-log` and `internets/kestrel` both report:

```
ALARM  absence   no manifest at all: nothing has completed a night since the substrate landed
```

Correct, and worth writing down: **the instrument's first honest reading of a live world is that it
has no evidence.** The alarm clears the first night that runs against the new binary — and if it
does not clear, that is the substrate failing rather than the world being quiet, which is the
distinction the whole file exists to preserve.

---

### I-009 · An adversarial review of the day's work found 25 defects

**found_by: code-reading** · both · severity: high

Six independent readers over everything above, each given one dimension and the real tree; every
finding then handed to a separate skeptic instructed to refute it and to default to refuted. 38
candidates, **25 survived**. Filed as one entry because the method is the finding.

The worst of them predates all of today's work and sits in the wedge: **`Budgeter::fit` evicted,
announced what it had freed, and the loop sent the untrimmed history anyway.** `fitted.evicted` was
read nowhere in the crate. Nothing failed — the run succeeded and the freed tokens were billed —
until a prompt overshot far enough for the provider to refuse it, from a framework that had just
said it had made room.

That one is worth separating from the rest. Every other honesty defect this project has recorded is
a sentence a person wrote and did not go back to. This one is *generated at runtime, once per turn,
by the code*, and it is the exact failure the whole programme exists to make impossible.

Two of the twenty-five were genuinely `found_by: system` and are already above — `marks::survey`
catching the OpenAI declaration (I-003), and the producer lint catching `Effect::InputSupplied` once
its swept list was widened. The other twenty-three were found by reading, which is the honest
number and the uncomfortable one: **the instruments built this morning found two of the defects in
the instruments built this morning.**

Six of the twenty-five were the artifact overclaiming itself — `claims.toml` rows pointing at tests
that did not establish them, including one settled by a test whose two sides come from the same
expression. In a repository whose deliverable is *not overclaiming*, an overclaim inside the claims
table is self-refuting, and it is the category that most needed an outside reader.

All twenty-five are fixed or recorded. What is not fixed is the underlying rate: everything here was
written and self-checked in one sitting, and one adversarial pass over it found this much. The
lesson is not "review more"; it is that **a check written by the same person in the same hour as the
code shares its blind spot**, which is the same reason the capability audit missed two adapters.

---

### I-010 · Three defects in the sweep's own output, found by reading its output

**found_by: code-reading** · frey/xtask · severity: medium

The full ten-target sweep ran, the table was generated, and reading the generated table found three
things about the generator. All three are the same shape as everything else in this file: **a thing
that presents as a measurement and is not one.**

1. **The four servers that never started were printed as `handshake`, `0`, `0`, `0`, `no`.** The
   `stateless` column asserted a protocol fact about a Python process that died on an `ImportError`
   before reading a byte, and the three zeros beside it read as a clean sweep across zero tools. Now
   `—` in every column a dead process cannot answer for. The headline was already correct — *0 of 6
   servers **that answered*** — so the file disagreed with itself four rows above its own conclusion.

2. **`results.jsonl` was written with `fs::write`.** It truncated. A file named for a line-per-record
   format held exactly one line, and every earlier sweep had been erased by the next one — which
   makes the sentence this whole section is built on, *"the number to watch over the next six months
   is that column going from 0 to non-zero"*, a number nobody could watch. There was no history to
   compare against. The claims checker reads the **newest** `day` across all lines and had been
   right all along; the writer was wrong. Now appends, replacing same-day re-runs so three passes
   while fixing a renderer leave one row rather than three that look like observations.

3. **The churn line printed both twelve-item lists** and left the reader to diff them by eye. It now
   names the delta — *gained `simulate-research-query`* — which is the entire bug report.

Two tests pin the first two. The third is cosmetic and has none, which is the honest split.

**Why this one is worth its own entry rather than a line in I-009.** Every defect in I-009 was found
by six readers pointed at the code. These three were found by looking at *what the instrument
printed*, an hour later, while doing something else — and the second one is the recurring shape at
its purest: **a declared capability (a dated time series) with no producer on the path that
mattered** (a writer that truncates). That is the fourth instance of that exact shape today, and the
producer lint cannot see this one, because the producer exists and does the wrong thing.

---

### I-011 · The MCP client cannot connect to anything, and the sweep that "proved" it could never used it

**found_by: code-reading** · frey · severity: high

Asked a question about MCP startup latency. Went to look at the client's connect path. Found three
things, each of which makes the next one worse.

1. **`McpClient<T: Transport>` ships no `Transport`.** Both implementations in the repository —
   `FakeServer` and `Loopback` — are inside `#[cfg(test)]`. No stdio, no HTTP. A caller gets the
   protocol logic and writes the pipe themselves. That is a defensible library design; it was never
   said out loud, and the docs read as though you could connect.

2. **There is no client-side shim for older servers.** `negotiate()` correctly identifies a
   pre-stateless server from its method-not-found and writes `stateless: false` into
   `ServerIdentity`. **Nothing reads that field.** `initialize` and `notifications/initialized` are
   never sent by the client, anywhere. Since the sweep established that **0 of 6** reachable
   third-party servers speak the stateless revision, the client would list nothing from any real
   server tested. The README and `docs/mcp.md` both advertised the shim.

3. **`cargo xtask conformance` does not use Frey's MCP client.** `xtask` does not depend on
   `frey-mcp`. It hand-rolls JSON-RPC over stdio — which is *why* it works, and why nobody noticed
   the client could not. Its module doc opened with *"Connect Frey's MCP client to servers Frey did
   not write."*

**The compounding is the finding.** Any one of these is ordinary. Together they produced a claim —
`mcp.works-with-servers-frey-did-not-write`, status `operated`, the **only** `operated` row in the
file — resting on evidence that never executed the subject of the claim. It went into the 0.2.0
release notes and the README status line this morning, roughly six hours before this entry.

The instrument was pointed at the ecosystem and the label said it was pointed at us.

**What was true and is kept:** the sweep's actual measurement. Ten servers, six reachable, zero
stateless, one churning, 67 tools. That is a fact about the ecosystem, a hand-rolled client is a
perfectly good instrument for measuring somebody else's server, and it is now
`mcp.the-ecosystem-still-needs-a-handshake`. The **server** direction is real, tested, and
untouched by all of this — including its own `initialize` shim for pre-stateless *clients*, which
is a different mechanism that does exist.

**Why the producer lint missed it.** It sweeps enum variants — `Warning`, `EventKind`, `RunError`,
`Item`, `Effect` — and `ServerIdentity::stateless` is a struct field. Same defect shape as I-001 and
I-003, one type constructor away from the detector built for it. A field-level orphan check would
catch this class and does not exist.

**Three claims retracted**, one added, and `claims.toml` goes from 1 `operated` row to 1 different
`operated` row. The count did not move; what it rests on did.

---

## Open

- **No `frey_rev` in any historical record.** Everything before today is attributable to "frey",
  which changed six times in one day. Not fixable retroactively; fixed forward.
- **The 30-night clock has not started.** It is gated on a dedicated OpenRouter key with a hard
  monthly ceiling, which is an operator action. Everything else it needs is built.
