<!-- Produced 2026-08-14 by a 13-agent workflow: four independent ground-truth sweeps, four
     evidence programmes designed from different angles, an adversarial critique of each, then one
     synthesis. 1.47M tokens. The load-bearing claims below were re-verified by hand afterwards —
     see "Verified by hand" at the foot of this file for what was checked and what it showed. -->

# The Frey Production-Evidence Plan

## The finding that reorders everything

Before ranking anything: I re-ran the greps and one result changes the shape of the plan.

`crates/frey-providers/src/openrouter.rs` contains **no reference to `cache_control` or `request.marks`**. Its `capabilities()` (line ~66) hardcodes `CacheSupport::Automatic { min_prefix_tokens: 1_024, explicit_available: false }`. `CacheSupport::breakpoint_budget()` (`frey-core/src/provider_caps.rs:79-85`) returns **0** for that shape, and `CachePlanner::plan` takes the early return at `frey-context/src/cache.rs:132-145` — before churn detection, before the min-prefix check — returning `marks: []`.

deadnet uses exactly this dialect (`deadnet-cli/src/provider.rs:73`). So:

- Every cache plan in every live session deadnet has ever run is **empty by construction**.
- `Warning::CacheChurn` and `Warning::BelowMinPrefix` are **structurally unreachable** on the only path anyone runs.
- `profiles::openrouter_explicit()` has zero callers outside `all()` and its own test — no dialect's `capabilities()` ever returns it.
- `anthropic.rs:154 apply_cache_marks` takes `marks.last()` **only**, collapsing a 4-mark Opus plan to one, and returns early when `system` is empty. The doc comment at :107-109 ("a mark landing in the tool block becomes a marked tool") is false — `encode_tool` emits no `cache_control`.

The framework's headline claim is not *unmeasured*. It is *unrouted*. Every dollar spent A/B-testing the planner before this is fixed buys a null wearing a measurement's clothes. This is why the plan below does not run the economics A/B for 90 days, and why the first week is greps and docs rather than probes.

---

## Ranking the four proposals

**1st — Proposal 4 (skirnir / claims.toml).** Best survival rate per item. It contributed `claims.toml` — the right long-lived artifact, and the highest career-value thing here: a public claims table where a third of the rows say UNEVIDENCED, maintained by the same person who wrote the audit. It found the two divergent min-prefix tables (`profiles.rs` decorative, `anthropic.rs:38 min_prefix_for` live, with a `_ => 1024` fallback that silently catches Sonnet). It contributed the generalisation of "if an adapter claims a capability, assert the *request* enables it" from a one-off to a per-dialect check. And the `estimate_tokens` reconciliation (`run.rs:650` is `len/4`, while the provider returns `prompt_tokens` in every response Frey already decodes) is the single best unprompted idea in the whole set — free, and the precondition for any token-threshold work ever being meaningful. Its own probe repo was mostly wrong; the critique of it was the deliverable.

**2nd — Proposal 3 (coroner).** Contributed **pre-registration as a method** and the **producer lint**, which is the only pre-registration that survived contact with the tree — all four predictions still hold (`Warning::RouteChanged`: declaration + Display arm, zero constructions; `RunError::NeedsInput`: zero constructions anywhere; `EventKind::Discovered`: one hit, in a test; `Item::Discovery`: zero). Its own kill conditions were half stale — `to_jsonl` already writes events as `{"event": …}` lines with a round-trip test at `journal.rs:424`, and `RequestFingerprint` is `{model, turns, tools}` with no clock — which is itself the lesson the plan absorbs: pre-register against code you have opened this week.

**3rd — Proposal 1 (canary).** Contributed the airlock instinct, the `spend` UPSERT correction (PK is `(sim_day, account, model)`, STRICT), the dead-key 402 trick, the n=5→n=30 correction on the guidance A/B, and the finding that **there is no spend cap anywhere in Frey** while deadnet hand-rolled one at `main.rs:634-644`. Weakened by scheduling everything on GitHub cron with no dead-man's switch.

**4th — Proposal 2 (Wire).** Its centrepiece was a from-scratch Rust reimplementation of `D:\first_responder`, which is running right now (`data/first_responder.db`, 259 MB, written 11:15 today). That is disqualifying for the vehicle. But it contributed the single best strategic correction in the set — *do not start a new operating clock; attach to one that already has a record and a dependent* — and **scheduled fault injection**, which is the only thing that gives `found_by: system` a denominator that is not luck.

---

## 1. The first thing, this week

**Day 1, before any code — three moves, none of which require a rebuild:**

**(a) The rebuild airlock.** Four `deadnet.exe` are running as I write this; `D:\deadnet\target\debug\deadnet.exe` and `target\release\deadnet.exe` both exist and are locked. Any `cargo build` in `D:\deadnet` fails at link with `os error 5` while they run. Protocol, written into `D:\deadnet\CLAUDE.md`:

- Frey edits and `cargo test` in `D:\frey` are free — they use `D:\frey\target` and touch nothing deadnet holds.
- deadnet rebuilds go to `CARGO_TARGET_DIR=D:\deadnet\target-next`, which links fine against a running exe. The new binary is promoted into place **only in a declared window** between nights.
- The nightly cron gets a written idle gap (e.g. no nights 09:00–10:00 local) and that gap is the only time the live exe is swapped.
- **Do not pin frey by git rev.** Proposals 1 and 2 both wanted this; proposals 3 and 4 correctly refused. The path deps at `deadnet-cli/Cargo.toml:32-34` and `deadnet-run/Cargo.toml:21-22` are the loop that produced all 13 bugs. Get attribution without coupling: a `build.rs` that runs `git -C ../../../frey rev-parse HEAD` and emits it as a const, stamped into every ledger row, every `spend` row, and every journal filename. Same credibility, zero severance.

**(b) A dedicated, hard-capped OpenRouter key.** Everything scheduled runs on a key with a **$10/month provisioning ceiling**, separate from the key deadnet's day-to-day uses. Frey has no spend cap (verified: no `max_cost`/`budget_usd`/`spend_cap` anywhere in thirteen crates), the operator's own notes record that thinking models bill full `max_tokens` and return empty content, and nothing scheduled should ever be able to drain the balance that deadnet's budget governor depends on. Five minutes. Non-negotiable precondition for anything on a timer.

**(c) Write up the cache-marks finding and correct the docs.** `README.md:3`, `docs/context-and-caching.md`, and `docs/providers.md` currently imply the planner governs every provider. The true statement: *marks are realised on one dialect of three; on OpenRouter the planner returns an empty plan by design and two of its three warnings cannot fire; `check_lookback` (`run.rs:371`, `cache.rs:283`) takes no capabilities and applies Anthropic's 20-block figure to every provider.* Add a `frey doctor` line reporting **mark support per dialect**.

**Why this first:** (a) makes every subsequent step safe against the one live system; (b) bounds the tail on the only irreplaceable resource; (c) stops the framework from spending money measuring a subsystem that is not connected. None of the three requires a build, so all three land today.

---

## 2. New repos: **zero**

All four proposals wanted one — `skirnir`, `coroner`, `wire`. That unanimity is not signal; it is four different repos for four different reasons, which means none is load-bearing.

- **The MCP conformance sweep** lives at `D:\frey\xtask\conformance` with results committed to `D:\frey\notes\conformance/`. Nothing dies. A stranger running `cargo install skirnir` is a fiction — there are no strangers.
- **The probe/canary** is a GitHub Actions workflow in `frey` writing to `frey/notes/evidence/*.jsonl`. Nothing dies.
- **The news deployment (Wire)** does not get built. `first_responder` exists, runs, has 22 days of uptime, a published KPI, a supervisor with a crash history, and a downstream dependent (lazier). Building a second one dies, and should.

The operator has fourteen projects in memory and one income of $0. A new public repo is a standing maintenance obligation priced at zero in all four proposals. Refusing all four is the highest-leverage decision in this plan.

---

## 3. What runs continuously, where, cost, alerts

**Where:** the Windows laptop, at least until day 90. There is no VPS. Two of the four proposals wanted a Hetzner box; the honest version of that argument is *the laptop is not an unattended host and Windows Update will reset the streak* — and the right response is to **record that as an incident when it happens**, not to pre-buy infrastructure. An operating record whose first entry is "day 9, host rebooted, detected by dead-man's switch in 26 min" is better evidence than one with no entries.

**What runs:**

| Thing | Cadence | Cost/mo |
|---|---|---|
| deadnet nights, 40 identities | nightly, scheduled task | $10.20 |
| Fault-injection drill against a **copied** world | weekly | ~$0.50 |
| MCP conformance sweep (no inference) | weekly, GH Actions | $0 |
| Producer lint + claims.toml check | every push | $0 |
| Dead-man's switch (healthchecks.io free) | 26h window | $0 |
| **Total steady state** | | **~$11/mo** |

One-offs: ~$2 for the live probes (AgentCli against the real `claude` binary the operator already has; a real 402 from a deliberately exhausted $1 key), ~$2 for the guidance A/B at n=30, and a **capped $25** for the cache A/B if and only if it becomes measurable (§ day 90).

Realistic first month including development spend against live APIs: **$25–35.** Worst month if the A/B runs: **$50.** This is 3–10× under every proposal.

**What it alerts on** — and critically, only signals that can physically fire on the live path. Drop the `CacheChurn`/`BelowMinPrefix` rules every proposal wanted; they are unreachable through OpenRouter:

1. **Absence.** No night-completion ping in 26 hours → ntfy. This is the only alarm that catches the instrument itself dying, which is the failure every proposal's monitoring design had.
2. `cost_micros == 0` on more than 5% of sessions — the exact regression shape of the reverted `usage: {include: true}` bug.
3. Dead-turn rate above the trailing 7-night median + 3σ (the `gpt-oss-120b` 4/5→0/5-within-an-hour canary).
4. `stop = TurnLimit` rate, and **any run exiting with no `RunFinished` event at all**.
5. Zero tools presented to any agent in a session (the `ToolHost::definitions` silent-shrink failure).
6. Night cost > 2× trailing median.

---

## The order of work

### Week 1 — $0, no rebuild of deadnet required

- The three Day-1 moves above.
- **Producer lint** in frey CI: fail on any public enum variant with a `Display`/doc surface and zero constructions outside tests. It goes red on `RouteChanged`, `NeedsInput`, `Discovered`, `Item::Discovery` on first run. Resolve each deliberately: **write the `RouteChanged` producer** (one line — `run.rs:339` already has `self.model` and `response.model` in scope, and `Response` carries `provider` precisely so router substitution is visible); mark the other three as TODO rows in `claims.toml` rather than deleting.
- **Three event-stream fixes in `run.rs`**: populate `RunFinished.cost` (:380 hardcodes `None`; the expression exists at :386); emit `RunFinished` on the turn-limit path (:520) **and on the provider-error path** (`self.provider.complete(request).await?` at :336 — the bigger hole, and the one that fires at 3am); replace `warnings.dedup()` (:384) with a seen-set retain, since the two append sites at :306 and :310 guarantee interleaving and make the doc comment two lines above it false.
- **`claims.toml`** at repo root, one row per claim in the audit, with `status ∈ {settled, operated, tested-only, declared-only, retracted}` and `settled_by`. The CI check must resolve `settled_by` to a **named test CI observed passing** or a results row with a timestamp inside a staleness window — not file existence, which goes green forever while claims rot.
- **MCP conformance sweep** against ten real third-party servers. $0, no inference, one day. Frey has never connected to an MCP server it did not write; every claim in `docs/mcp.md:27-38` rests on `FakeToolset`. Publish the aggregate table and frey-side issue numbers; disclose per-server defects to maintainers privately before naming anyone.
- **Live probes, 30 minutes, $2**: run `AgentCli` against the real `claude` binary on this machine (ten minutes, converts a headline feature from "treat as untested"); exhaust a $1 key and watch 402 be fatal rather than turning every remaining turn into a billed no-op.

### Week 2 — the durability substrate (deadnet, one rebuild window)

This is the strongest single piece across all four proposals and the whole plan is measured through it.

- **`spend` UPSERT** at `night.rs:719-726`, where `cost_micros` is already computed and thrown into a println. The table exists with exactly the right columns in all six worlds and has **0 rows in every one**. Must be `ON CONFLICT (sim_day, account, model) DO UPDATE SET tokens_in = tokens_in + excluded.tokens_in, …` — a plain INSERT errors on a re-run night, inside the night it was recording. Must be best-effort: a failure to record spend can never abort a night. Write with `BEGIN IMMEDIATE`.
- **Make `cost_micros` nullable-or-flagged.** `map_or(0, …)` writes $0.00 when the provider reported nothing — indistinguishable from a free model, and it is the exact regression alarm #2 exists to catch.
- **`Journal::to_jsonl` to `internets/<world>/journal/`** — the directory is created at `world.rs:41`, documented at `DESIGN.md:69`, and holds **0 files in all six worlds**. `to_jsonl` already round-trips events correctly (`journal.rs:175-186`, tests at :424 and :448), so this is a write call, not a serialiser fix. Rotate/gzip from the first commit.
- **`tracing-subscriber` + JSON layer, `EnvFilter` pinned to `frey=info`**, rotated. deadnet has no `tracing` dep at all today; Frey already emits `info_span!("frey.turn", turn, model, "gen_ai.system")` at `run.rs:283-288`. ~20 lines, and every existing run becomes a durable trace with no change on either side. Without the filter, seven unattended days of reqwest/hyper spans fills the disk and kills the run this exists to observe.
- **Stamp the frey rev** from the build script into every spend row, journal filename, and `bench/ledger.md` row. This retroactively makes the 433-line ledger — the best public artifact in either repo — attributable to a framework version. It currently is not.

### Weeks 3–4 — make the record trustworthy before starting the clock

- **`ToolHost::definitions` async and fallible.** Four independent authors wrote `unwrap_or_default()` or a silent `continue` (`deadnet-run/src/lib.rs:123-125` drops any tool whose schema will not parse, with no error, no warning, no event). deadnet's own PLAN rates it "high, blocking for M3's unattended nights"; frey's audit calls it A4; dogfood ranks it #1. The deeper seam nobody documented: `definitions()` is **both the presented list and the identity of the cached prefix**, so a shrunken catalog silently changes what the cache key means. Do it now, with four consumers, in a declared rebuild window.
- **`estimate_tokens` reconciliation.** Add a per-response comparison of `len/4` against the `prompt_tokens` Frey already decodes, recorded to the trace. Free, uses data already in the response body, and it is the prerequisite for any min-prefix work being anything other than a measurement of Frey's own estimator error (±1000 tokens at Haiku's 4096 — sixteen times the margin every min-prefix probe proposal used).
- **Fault-injection harness.** Weekly, against a **copy** of a world, never the live one. Five faults: lock the corpus DB; return 402; perturb a cached prefix by one byte; have a tool return `NeedsInput`; feed an oversized listing. Record whether an alarm fired and how long it took. This is what turns "30 nights, 0 incidents" from a claim about health into a claim about the instrument.
- **`Agent::replay` entry point**, re-registered against the tree. `Replay::next_response` genuinely has zero callers outside `journal.rs` and `Agent::run` never mentions it. But the interesting result is not the one anyone predicted: `RequestFingerprint` is `{model, turns, tools}` — **content-blind**. A journal replays *green* after the system prompt changes. Publish that: divergence detection catches shape changes and not content changes, which qualifies "replay diverges loudly at the first mismatch" honestly and costs $0.

### Weeks 5–12 — the operating record

The 30-night clock starts once the substrate and the `definitions` fix are in. **Explicitly decoupled from deadnet's M3 gate.** Proposal 1 was right that M3 needs nine unbuilt features (arrivals, mail, crawler, who.tsv, diaries, off-tick chat, skills, cred billing, twenty non-founders); proposal 4 was right that ten people is a toy. Neither is the evidence run. The evidence run is **the existing night loop, 40 identities, on a scheduled task, for 30 consecutive nights, with nobody watching**. It adds no deadnet feature and it is the only thing that settles "runs unattended."

Alongside: the **guidance A/B at n=30/arm on two models**, run **inside deadnet against F4** where the toolset, the real `deadnet_site::validate`, and `Efficiency::from_journal` (`f4.rs:328-384`) already exist. Do not build a toolset in frey to run an experiment — that was proposal 4's critique's correct objection to doing it in-repo, and running it in deadnet dissolves the three-session estimate to one. Lift only the result plus the frey rev into `frey/notes/evidence/`. ~$2.

### Day 90 — the one gated experiment

The economics A/B, if and only if marks reach a wire. Scope the fix narrowly: resolve `OpenRouterDialect::capabilities` per model for the `anthropic/*` family only (where `cache_control` passthrough is documented), behind an explicit opt-in constructor so the "a hardcoded table of upstream quirks rots" comment in `openrouter.rs` stays true for the default path. That makes `openrouter_explicit()` reachable for the first time. Also fix `anthropic.rs:154`: honour all marks, not `.last()`; do not drop them when `system` is empty; mark the tool block.

Then, **capped at $25**, on an Anthropic-family model:
- Control arm = **one hand-placed breakpoint after the system prompt** — what a tutorial says and what a model writes. Not zero breakpoints. Three of the four critiques independently called the zero-breakpoint arm a strawman that decides the result before the first call.
- Primary metric = `cache_creation_input_tokens` / `cache_read_input_tokens`, which are ground truth. **Not dollars** — `anthropic.rs:78` is `reports_cost: false`, so any dollar figure would be validated by Frey's own hardcoded price table, which is the rotting-constant class the programme exists to catch. Reconcile dollars against the real invoice once.
- Pre-register the threshold, and **publish the null if it is null**. A planner that ties a hand-placed breakpoint on realistic traffic is the most valuable result available and the only one the other designs made unreachable.

This breaks the standing "no Anthropic in the user's projects" rule for one bounded experiment. That needs an explicit operator decision, not a quiet exception — and it is precisely why deadnet can never witness the headline claim.

---

## 4. Evidence at 30 / 90 / 180 days

**Day 30 — checkable:**
- `select count(*) from spend` returns > 0 in the live world; ≥ 20 sim-days of rows with model, tokens, cost, dead-turns.
- `internets/<world>/journal/` holds ≥ 500 `.jsonl` files, each with a frey rev in its name.
- frey CI fails on a new zero-producer variant; `RouteChanged` has a producer; the other three carry TODO rows.
- `frey/notes/conformance/` has a table covering ≥ 8 third-party MCP servers, with ≥ 1 frey issue filed off it.
- Docs corrected on marks-per-dialect, `check_lookback`'s provider-independence, and the two divergent min-prefix tables.
- `claims.toml` merged, CI-checked, with the honest count published: roughly 12 settled, 6 operated, 10 tested-only, 6 declared-only.
- 3 fault drills logged with detection latency.
- `AgentCli` run against a real `claude` binary; a real 402 observed fatal.
- Spend to date: ~$25.

**Day 90:**
- 60+ nights in the ledger. `notes/INCIDENTS.md` with an entry per failure and a `found_by: {system, operator, code-reading}` field on each — **≥ 3 found by the system**.
- Estimator error distribution over ≥ 10k real calls, published.
- A real deadnet journal replayed to completion; the content-blindness finding published.
- Guidance A/B at n=30, two models, ±2·SE, in `bench/ledger.md` with a frey rev.
- 12 fault drills, detection-latency table.
- `README.md:17-18` replaced: *"It has run unattended for N nights across M sessions on one machine. Here is the operating record; here are the K failures; here is which channel found each."*
- Spend to date: ~$50.

**Day 180:**
- 150+ nights; ~6 months of `spend`, journals and incidents.
- The cache A/B result, with honest arms — or a written statement that it was not run and why.
- The approval hook on `Agent` with a `RunError::NeedsInput` producer, witnessed by the fault drill: a tool returns `NeedsInput` and the loop **surfaces** it rather than rendering "approval was not available" and continuing (`audit §A3`).
- `claims.toml` with a measured churn number: how many rows went green→red on their own between day 30 and day 180. That number is the whole thesis.

---

## 5. Interaction with deadnet

deadnet is the only real user and it is live: four `deadnet.exe` running, six worlds on disk, `world.db-shm` touched today. Rules:

- **The airlock (§1a) governs every deadnet-affecting change.** `CARGO_TARGET_DIR=D:\deadnet\target-next` for builds; promotion only in the declared window. Frey's own `cargo test` never touches deadnet's target and is unrestricted.
- **No schema migration.** `deadnet-store/src/schema.rs:26-27` says in terms that the schema is one string and not a migration chain "because there is no deployed world to migrate yet" — there are six, and they are STRICT. The `spend` UPSERT and the journal write need **no** schema change, which is exactly why they are first. Do not add a `frey_rev` column; put the rev in the row content and in the journal filename.
- **Nothing new writes to `world.db` inside the night loop that can fail the night.** Both new writes are best-effort with a warning.
- **Do not pin frey by rev.** Stamp it.
- **Do not run fault injection against the live sim.** Copy the world.
- **Do not touch `site::publish`'s signature** to demonstrate taint. The census is the deliverable — `Tainted`/`Trusted` appear in deadnet exactly five times, all `Provenance::new("test")` in `deadnet-run/tests/wiring.rs`, which makes `DESIGN.md:395` and `QUALITY.md:94` false as written. Correcting them is the evidence. One authored payload reaching one authored sink is a screenshot.
- **Separate OpenRouter keys.** A canary retry storm must not 402 deadnet's next night, which the operator's own notes record as degrading silently.

---

## 6. What this deliberately does not do

- **No new repos.** Every proposal wanted one; each wanted a different one; none is load-bearing. Zero marginal evidence, permanent maintenance.
- **No VPS for 90 days.** An idle axum server staying up is evidence about Hetzner. Revisit only if `INCIDENTS.md` says host instability is the dominant failure mode — at which point it becomes a prerequisite with a real argument, not a badge.
- **No Wire, no news product.** `first_responder` exists and runs.
- **No cache A/B before day 90.** On OpenRouter both arms emit identical bytes; publishing that null would be the single most misleading artifact available.
- **No live min-prefix probe** until the estimator is reconciled. A two-point ±64-token assertion sits inside a ±1000-token estimator error and would go red for Frey's own arithmetic, published as a vendor-constant failure. The free half — two divergent tables, one decorative — ships in week 1.
- **No skills / progressive disclosure / `Item::Discovery` work.** deadnet's Decision 37 declines it *by name* ("we would not want it: a fixed tool block is the cacheable shape") and is therefore evidence against it for high-volume short-session workloads. Half the stated wedge stays a design document, honestly labelled in `claims.toml`.
- **No sandbox enforcement backend, no A2A transport, no AG-UI stream.** Already retracted or correctly non-claims.
- **No chase of deadnet's M3 feature list.** The operating record needs the night loop on a timer, not arrivals, mail and a crawler.
- **No spend-cap feature in Frey** in the first 90 days. A per-key OpenRouter ceiling is free and real; the framework feature is a nice-to-have that five projects have now hand-rolled and can wait for a design.

---

## 7. Biggest risk, and the tripwire

**The risk is not money and not engineering. It is that the durability substrate lands and nobody reads its output.** The operator runs deadnet, first_responder, hackernews, babelfish and tooler; he has $0 income and a job hunt. The plan's specific failure mode is that `spend` fills with rows, `journal/` fills with files, the ledger goes green, and it becomes the same unread thing the `println!` was — at which point the plan has bought a more expensive way to not notice.

**Tripwire, and it fires early: if by day 21 there is no `INCIDENTS.md` entry with `found_by: system`, the instrument is dead, not the system healthy.** The historical record makes zero incidents in three weeks implausible — a 145-tool-call runaway that cost 10× a normal run and was found by reading an invoice; `gpt-oss-120b` going 4/5→0/5 within an hour on unchanged code; four provider errors in one 98-person night; two afternoons wedged on a free route with nothing on screen. Forty identities a night across eleven models for twenty-one nights will break something. Silence means the alarms cannot fire — which, given that two of the three warnings every proposal wanted to alert on are structurally unreachable on this path, is the default state until proven otherwise.

**Second tripwire, weekly:** two consecutive fault drills undetected → stop building and fix the alarms. The drills are the only reason a green dashboard means anything.

**Third, on the plan's honesty:** the brief's definition of production has three parts and savings can buy two. "It runs unattended" and "the system tells you" are purchasable for $11/month. "Something real depends on it" cannot be manufactured — deadnet is the operator's own project, and a careful reader will file all of this as sophisticated dogfooding. That is worth a great deal and it is not production. The honest deliverable at day 90 is not the word "production." It is replacing `README.md:17-18` with a paragraph precise enough that a staff-level reader can check every number in it — and `claims.toml` standing behind it, with a third of its rows still saying UNEVIDENCED.

---

## Verified by hand after the workflow

Agent reports get checked before they change anything. Every load-bearing claim in this plan was
re-run directly against the tree on 2026-08-14, at `main` = `60906a5`:

| Claim | Verified | Result |
|---|---|---|
| `openrouter.rs` never references `cache_control` or `request.marks` | `grep -c` | **0 references** |
| `breakpoint_budget()` is 0 for `Automatic { explicit_available: false }` | `provider_caps.rs:79-85` | falls to `_ => 0` |
| OpenRouter declares exactly that shape | `openrouter.rs:68` | confirmed |
| `CachePlanner::plan` returns before churn detection at budget 0 | `cache.rs:132-145` | confirmed |
| `Warning::RouteChanged` has no producer | `grep` outside `event.rs` | **0** |
| `warnings.dedup()` removes only consecutive duplicates | `run.rs:384` | confirmed; the comment two lines above claims otherwise |
| `RunFinished` hardcodes `cost: None` | `run.rs:380` | confirmed |
| `apply_cache_marks` honours only the last mark | `anthropic.rs:155` | `request.marks.last()` |
| deadnet's `spend` table is empty | 6 worlds queried | **0 rows in all six** |
| deadnet's `journal/` directories are empty | 6 worlds listed | **0 files in all six** |

The last two are the operationally significant pair: deadnet has been running live and **recording
nothing durable**. The tables and directories exist, are correctly shaped, and have never been
written to. Everything this plan calls "the substrate" is that gap.

The first four are the pair that reorders the plan, and they are worth stating plainly rather than
leaving inside a table: **on the OpenRouter dialect the cache planner returns an empty plan by
construction**, before churn detection and before the minimum-prefix check. That is defensible as
designed — OpenRouter caches automatically, so there are no explicit breakpoints to place, and
`provider_caches_automatically` is set truthfully. Caching *is* happening; the first live run this
project ever made reported 864 cached tokens.

What is not defensible is the consequence. `Warning::CacheChurn` and `Warning::BelowMinPrefix`
**cannot fire on that path**, and deadnet's `ECONOMICS.md` §1 names churn as the single largest
threat to its budget and says in terms that *"frey's cache planner exists for exactly this and will
warn rather than silently bleed."* On the dialect deadnet runs, it cannot warn. Churn detection does
not need a breakpoint budget — a segment that claims to be stable and changed costs money on an
automatic provider too, arguably more, since you cannot even place a breakpoint to mitigate it. That
is a small fix in `cache.rs` and it belongs in week 1 rather than at day 90.
