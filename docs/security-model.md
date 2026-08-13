# Security model

Written for the case where someone is going to audit this. The design goal was that a harness built
on Frey inherits audit-friendly properties rather than acquiring them afterwards.

**Prompt injection is not solved**, by Frey or anyone. Everything below reduces blast radius.

## Untrusted data is a type

Everything from outside — a tool result, a fetched page, an MCP server's own description, a peer
agent's reply — is `Tainted<T, Low>`. Passing it somewhere that needs trusted input does not
compile:

```
expected `Tainted<String, High>`, found `Tainted<String, Low>`
```

Those forbidden flows are `compile_fail` doctests, so the **compiler** proves them on every platform
rather than a snapshot of diagnostic text pinning them to one toolchain.

Two axes: integrity (`Low`/`High`) and confidentiality (`Public`/`Secret`). `zip` meets integrity and
joins confidentiality, which is the rule that stops a combination of one trusted and one untrusted
value from reading as trusted.

### Raising integrity

`endorse()` is the only way, it is `#[track_caller]`, and it is audited. So the auditor's question —
*"where does untrusted data become trusted?"* — is answered by `grep endorse` plus a log with file
and line, rather than by reading the whole codebase.

Prefer `validate::<V>()`, which narrows the type **and** raises integrity in one step. Parsing is a
better trust boundary than a human deciding something looks fine, because the narrowed type carries
the decision forward.

## Capabilities

A tool reaches the world only through capabilities it declared. There is no ambient filesystem
handle, no HTTP client, and no environment in `ToolCx`.

```rust
PathScope::new(["./src", "./tests"])
HostPattern::new("api.github.com")
ProgramScope::new(["git", "cargo"])
```

> A real bug found by a test here, kept as a permanent regression: `PathScope::new(["./"])`
> normalised to `"/"`, so a policy that read as *the workspace* granted the entire filesystem.
> Nothing looked wrong, which is what made it the worst kind.

**Grants only ever narrow.** A child agent's capabilities are a subset of its parent's, checked at
spawn rather than at use, because by the time a capability is exercised the decision has been made.
A descendant of an empty grant set can acquire nothing however deep in the tree, and there is a test
at depth saying so.

Untrusted input flows downward too: a parent that has read a fetched page cannot hand a child a
clean slate by summarising it, since the summary derives from that page.

## Sandboxing

> **Read this before anything below it.** `frey-sandbox` is a **policy language and a decision
> procedure**. It is not a sandbox. `SandboxBackend` is a trait with no implementation, no syscall is
> made on any platform, no Landlock ruleset is applied and no Seatbelt profile compiled — and
> nothing is confined because nothing is executed: Frey ships no tool that spawns a process. An
> earlier version of this page presented the table below as implemented. It was wrong, and the
> [capability audit](../notes/audit/01-capability-audit.md) §A2 records how it got that way.

What `frey-sandbox` does today is decide, and report:

- `policy::validate` — would this exec be permitted by this policy?
- `policy::decide` — what confinement is required, and is it available?
- `probe::*_availability` — what a given platform state affords. Note that these take the ABI level
  and the `lsm=` flag **as parameters**; they report, they do not detect.

That is genuinely useful — it is the part you want audited, it is pure, and it is testable on every
platform including the degraded paths a healthy CI machine cannot reproduce. It is also, on its own,
worth nothing at runtime until a backend exists.

The mechanisms a backend would use, when one is written:

| Platform | Mechanism | Requires |
|---|---|---|
| Linux | Landlock | kernel 6.12 *and* `landlock` in the `lsm=` boot parameter |
| macOS | Seatbelt | — |
| Windows | AppContainer / restricted token + Job object | — |

The design decisions below are real and hold whenever a backend arrives — in particular that partial
confinement is reported as partial. Landlock ABI 1 scopes the filesystem but not ports, and reporting
that as "unavailable" would push an operator toward disabling confinement entirely, so the refusal
names exactly which control is missing.

> **The ABI level is also not detected by syscall.** `doctor` reports the conservative answer rather
> than a number that might be wrong.

**`ProgramAllowlist` is enforced by Frey refusing to spawn, not by the kernel** — and it is the
control that matters most, because a program that never starts cannot escape anything.

The built-in validators show the shape. `sh -c "r''m -rf /"` fails on `sh`, before the payload is
considered, because the program is compared as a whole argv element and there is no command string
to obfuscate. `https://api.github.com@evil.test/` is refused because it reads as GitHub and resolves
elsewhere. An environment variable that looks like a credential is refused outright, since a sandbox
never holds a secret.

## Approvals

**The prompt shows the literal action** — the exact command, URL, or statement — never a
natural-language summary. A summary is precisely where an instruction injected upstream survives
review by the person clicking yes.

**Risk comes from the declaration.** A tool's own account of how dangerous it is, or the model's, is
not evidence.

**An absent answer is not a yes.** A resumed call carrying no response asks again rather than
proceeding. Treating silence as consent turns an approval gate into a formality that logs nicely.

**A headless surface with an interactive approval policy fails at build time**, rather than stopping
at the first gated action to wait for a human who is not there, discovered at three in the morning.

## Untrusted parties, by name

- **MCP servers.** Listings re-sorted, freshness hints capped at an hour, catalogs private by
  default, results labelled with provenance.
- **Agent cards.** A *verified signature does not make the text trustworthy* — verification changes
  who is responsible for the text, not whether it can be obeyed. The test that pins this uses a
  signed card carrying an injected instruction and asserts the text arrives indexed and
  low-integrity. An **invalid** signature is refused outright: worse than unsigned, because someone
  tried.
- **Skills.** A skill outside a trusted root reaches the prompt as low-integrity text whatever it
  says about itself, and **a skill cannot grant itself capabilities** — anything ungranted is
  refused at load, not prompted for mid-run, because a mid-run prompt is exactly where an injected
  instruction would like to be answered.

## Code mode bindings

Taken from Cloudflare's model: a binding is a **handle, not a credential**. Every call goes to the
supervisor, which holds the token. A script cannot leak a key it never had — an entire class of
failure removed rather than mitigated.

Only tools whose caller policy permits code may be bound, enforced by Frey rather than by the
provider, because Anthropic document `allowed_callers` as guidance to the model rather than as a
boundary.

## Where the checks live

Every tool call passes through the same layers — policy, validation, approval, redaction,
truncation, audit. There is no second path to executing anything, which is what makes those layers
unavoidable rather than opt-in.

> **A known structural weakness.** There are now three dispatch surfaces — the agent loop, the MCP
> server, and multi-agent spawn — and they enforce the checks independently. One of them was
> already found skipping validation (see
> [`notes/dogfood/01-demo-projects.md`](../notes/dogfood/01-demo-projects.md) D2). Nothing
> structurally prevents a fourth from repeating it. A single shared dispatch function is the real
> fix and is not done.

## The threat model, stated plainly

Frey assumes the model is **not** an authority. It may be manipulated by anything it reads, and
everything it reads is labelled accordingly. What Frey does not assume is that any of this is
sufficient: a determined injection that stays within granted capabilities will succeed, and the
mitigation is to grant less.
