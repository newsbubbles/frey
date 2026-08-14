# Evidence plan: what landed, and what is left

Against [`EVIDENCE-PLAN.md`](EVIDENCE-PLAN.md). The plan is laid out in weeks because that is how the
work depends on itself; **most of it is build work and the build work is done.** What remains is
calendar time and two operator actions, and neither can be compressed by writing more code.

## Week 1 — done

| Item | State |
|---|---|
| Rebuild airlock | `deadnet/CLAUDE.md`; builds go to `target-next`, promotion in a declared 09:00–10:00 gap |
| frey-rev stamping, without pinning | `deadnet-cli/build.rs` → every journal filename and manifest line |
| Cache-marks finding written up, docs corrected | `docs/context-and-caching.md`, `docs/providers.md`, `README.md` |
| `frey doctor` reports mark support per dialect | from a measurement, not a table |
| Producer lint in CI | `cargo xtask producers`; found 10, fixed 2, 8 acknowledged |
| `RouteChanged` producer written | plus `TurnStarted` and `EventsDropped` |
| Three event-stream fixes in `run.rs` | `RunFinished.cost`, `RunFinished` on every exit, seen-set dedup |
| `claims.toml` + CI check | 53 rows; the check resolves test names and dated evidence, not paths |
| MCP conformance sweep | 10 servers, `notes/conformance/`; two real findings |
| **Dedicated capped OpenRouter key** | **operator action — see below** |
| Live `AgentCli` probe, live 402 probe | not run; both need spend |

## Week 2 — done

The durability substrate, which the whole programme is measured through.

- `spend` upserted per `(sim_day, account, model)`, `BEGIN IMMEDIATE`, best-effort.
- `cost_micros` is an `Option`; `MANIFEST.jsonl` carries `cost_reported` so a `0` and a silence are
  distinguishable — which the STRICT schema cannot express and no migration was needed to fix.
- Journals written per person per night, frey rev in the filename.
- `tracing-subscriber` with the filter pinned to `frey=info`, rotated daily.

Verified by running it: `deadnet watch` on two live worlds correctly reports **absence**, because
nothing has run against the new binary yet. That is the honest first reading and it is recorded as
`I-008`.

## Weeks 3–4 — done

- **`ToolHost::definitions` is async and fallible.** `Err` fails the run; a reduced catalog does not;
  an empty one is reported whatever produced it. All five implementations updated.
- **Estimator reconciliation.** Every turn compares `len / 4` against the count the provider
  returned, to the trace, with a warning past 25%.
- **Fault-injection harness.** `deadnet drill`, five faults against a copy of a world. First run:
  5/5 detected, 10–14 ms. Recorded to `journal/DRILLS.jsonl`.
- **Replay is reachable from the loop** as `Replaying`, an ordinary `ModelProvider` — and the
  content-blindness finding is *fixed* rather than only published: `RequestFingerprint` carries a
  content hash, and a journal written before that reports `Divergence::Unknown` rather than a match.

## Day 90 — the code is done, the experiment is not

`OpenRouter::with_explicit_cache()` routes breakpoints to `anthropic/*` upstreams, which makes
`profiles::openrouter_explicit()` reachable for the first time. `frey doctor` now shows four
dialects, and the two that take breakpoints realise all four of them.

So **marks reach a wire** and the gate the plan set for the economics A/B is met — on the code side.
The A/B itself is not run and should not be run without an explicit decision, for the reason the plan
gave: it needs an Anthropic-family model, which crosses a standing rule about which models go into
these projects. That is an operator call, not a quiet exception.

When it runs, the arms are already decided and pre-registered here:

- **Control = one hand-placed breakpoint after the system prompt.** Not zero. Three independent
  critiques called the zero-breakpoint arm a strawman that decides the result before the first call.
- **Primary metric = `cache_creation_input_tokens` / `cache_read_input_tokens`.** Ground truth from
  the provider. **Not dollars** — `anthropic.rs` is `reports_cost: false`, so any dollar figure would
  be validated against Frey's own hardcoded price table, which is the rotting-constant class this
  whole programme exists to catch.
- **Capped at $25**, and **the null gets published if it is null.** A planner that ties a hand-placed
  breakpoint on realistic traffic is the most valuable result available here.

## What is actually left

**Two operator actions**, both five minutes, both blocking the clock rather than the code:

1. **An OpenRouter key with a hard monthly ceiling**, separate from the day-to-day one. Frey has no
   spend cap — no `max_cost`, `budget_usd` or `spend_cap` in any of thirteen crates — so the
   provider-side limit is the only real bound on a retry storm at 3am. `deadnet/ops/README.md` has
   the exact steps.
2. **A dead-man's switch** (healthchecks.io free tier). The only alarm that can catch the instrument
   itself dying; every other check reads a record a stopped process does not write.

Then `Register-ScheduledTask`, and the 30-night clock starts. Everything it needs exists.

**And calendar time.** Thirty nights is thirty nights. The day-30 and day-90 deliverables in the plan
are unchanged and every one of them is now a query rather than a project:

```powershell
.\target\release\deadnet.exe watch internets\the-log    # tonight
sqlite3 internets\the-log\world.db "select count(*) from spend"
Get-ChildItem internets\the-log\journal -Recurse -Filter *.jsonl | Measure-Object
```

## The honest ceiling, restated

Nothing above buys the word *production*. It buys "it runs unattended" and "the system tells you",
which the plan priced at about $11 a month and which are now built rather than planned. It does not
buy "something real depends on it", because deadnet is the author's own project — and a careful
reader will file all of this as sophisticated dogfooding, correctly.

The day-90 deliverable was never the word. It is replacing the README's status line with a paragraph
precise enough that a staff-level reader can check every number in it, with `claims.toml` standing
behind it and a third of its rows still saying UNEVIDENCED.

Today: **28 settled, 1 operated, 3 tested-only, 14 UNEVIDENCED, 7 retracted.**

The column that matters is the second one, and the single row in it should be read narrowly. It is
`mcp.works-with-servers-frey-did-not-write`, settled by `results:notes/conformance/results.jsonl` —
a dated record of frey's client connecting to ten third-party servers, which is genuinely a run
against something the repository does not control and genuinely not a test. It is also free: no
inference, no spend, no unattended hours. **It expires in 120 days**, and the honest reading of a
one-row `operated` column is that the cheapest possible operated claim is the only one this project
has earned so far. The rest still waits on the nights.
