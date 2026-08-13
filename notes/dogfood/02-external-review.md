# Dogfooding, day 2 — the first external reader

The first three demos were written by Frey's own author, which is a weaker test than it looks: the
same person who chose an abstraction is the worst judge of whether it reads. This is the first
review from someone building on Frey who did not write it — an agent planning
[deadnet](../../../deadnet), a large multi-agent simulation whose whole economic argument rests on
prompt-cache efficiency at ~35,000 sessions a day.

Their shape is unusual in exactly the way that finds bugs: **many short sessions sharing one stable
prefix**, rather than one long session. Everything below follows from that.

---

## Fixed here

### E1 — Routing affinity was keyed to the session, with no override.

`Agent` set `cache_key` to the session id and offered no way to change it. That reaches OpenRouter as
`session_id` and OpenAI as `prompt_cache_key`, both of which exist to send related requests to the
same upstream so they hit the same warm cache.

The default is *correct* for the case it was written against — one long conversation shares a
session and therefore shares a prefix. It is precisely wrong for the opposite shape. Thousands of
short sessions sharing a persona get a distinct key each, which scatters them across upstreams and
misses the cache the prefix exists to hit. OpenAI's key additionally needs sustained traffic to stay
warm, which no single short session produces.

Adds `Agent::cache_key(impl Into<SmolStr>)`. **The default is unchanged**, deliberately: the
original reasoning holds for the general case, so the unusual workload overrides rather than
everyone else absorbing a worse default. Three tests, including the one that is actually the point —
two runs with different session ids and the same persona key present the same key.

### E2 — Every `HttpProvider` built its own `reqwest::Client`.

Fine for a program with one provider; a trap for one with thousands. Each client carries its own
connection pool, DNS cache, and TLS session store, and constructing an adapter per agent multiplies
all of it by the population. The failure arrives as socket exhaustion, which does not look like a
client problem.

Adds `HttpProvider::with_client`. Infallible, because nothing in it can fail — the fallible
constructors are the ones that *build* a client.

The doc names the better answer first: `complete` takes `&self`, so one adapter behind an `Arc`
already serves any number of concurrent agents. `with_client` is for when the agents genuinely need
different dialects, endpoints, or credentials over one pool.

### E3 — The loop's own error guidance named an affordance the loop does not provide.

A call to a missing tool was answered with *"Use one of the tools that were listed, or search for
one."* Nothing in `Agent::run` consults tool search — `Bm25Search` and `RegexSearch` live in
`frey-context` and the loop never touches them. A model that takes the advice burns a turn proving
the tool is not there.

This is the project's own errors-point-forward principle pointed backwards, and it was in the one
place that teaches every model on every run. Now: *"Use one of the tools that were listed."* The
test asserts the new wording **and** the absence of the word "search", so the phantom cannot come
back.

---

## Held deliberately: `ToolHost::definitions`

The reviewer identified this as the worst of the findings and they are right. It is already ranked
first in [`01-demo-projects.md`](01-demo-projects.md) (D3), found independently by all three demos,
and deadnet is the workload that makes it bite: a per-site database hiccup on an unattended overnight
run surfaces as *the agent has no tools*, the model says so confidently, and that prose lands in a
corpus nobody is watching.

It is held anyway, for two reasons.

**It is a breaking change to a public API that shipped 0.1.1 the same day.** Not fatal, but not a
drive-by either.

**The fix contains a real design question.** Making it `async fn definitions(&self) -> Result<…>` to
match `Toolset` is the easy half. The hard half is what the loop does with the error:

1. fail the turn,
2. retry the listing, or
3. **run with a reduced catalog and tell the model so.**

The third is the interesting one and the only one consistent with this project's rule that
degradation is visible rather than silent — it would need a `Warning`, and a decision about what the
model is told so it does not plan against tools that are temporarily missing. That deserves thinking.

Recorded in deadnet's plan as blocking M3's unattended nights rather than M0, so there is room.

---

## Not bugs, but worth knowing if you are building at this scale

Verified while reviewing, and left alone because they are correct as designed:

- **Tool calls within a turn are serial.** `for (index, call) in calls…` with `.await` inside. Wall
  clock per turn is the sum, not the max. Fine for cheap tools; a ceiling for I/O-bound ones.
- **There are no concurrency primitives anywhere in Frey.** No `Semaphore`, `JoinSet`, or
  `tokio::spawn` in any crate. `multi::spawn` is a capability *check* returning a `Child` descriptor,
  not a task spawner. Scheduling, concurrency caps and spend caps belong to the caller. That is the
  right layering; it should just not be a surprise.
- **The loop never streams.** `ModelProvider::stream` exists and `Agent::run` only calls `complete`.
  Matters for a human-in-the-loop path, not for a batch one.
- **`run(task)` is system plus one user turn**, with no way to seed prior history. Carry-in belongs
  in the task, and that is also the *correct* place for it: putting per-session state in `system`
  would churn the cached prefix, which is the exact failure the cache planner warns about.
- **Definitions are fetched once per run**, outside the turn loop, while `frey-core`'s own doc says
  "once per step". For a stable-prefix workload the once-per-run behaviour is what you want, so this
  is a documentation defect rather than a behavioural one — but it does mean progressive disclosure
  is not wired into the loop, and a plan that assumes otherwise is planning for a feature that is
  not there.

## One piece of advice back

Pin Frey by `rev`, not by branch. deadnet is building a benchmark against a harness against a
framework that changed six times in a day, by the same author. Their own argument — benchmark to the
harness — extends one level further down than they took it.
