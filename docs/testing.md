# Testing an agent

`frey-testkit` exists so that your agent's tests cost nothing, never flake, and assert the things
that actually break.

```toml
[dev-dependencies]
frey-testkit = { git = "https://github.com/newsbubbles/frey" }
```

## A scripted model

```rust
use frey_testkit::scripted::{ScriptedModel, Turn as Scripted};

let model = ScriptedModel::new(vec![
    Scripted::tool_calls(vec![tool_call("fs_read", json!({"path": "src/main.rs"}))]),
    Scripted::text("It is an empty main function."),
]);

let run = Agent::new(model.clone(), Workspace, "scripted-model").run("what is in main.rs?").await?;
assert_eq!(run.text(), "It is an empty main function.");
```

Everything except the provider is the real thing: the context plan, the tool layers, the journal,
the ledger.

## Assert what the model was *shown*

This is the part worth using, and the reason the scripted model records requests rather than just
replaying answers.

```rust
let second = &model.saw()[1];
assert_eq!(second.tools.len(), 3);
assert_eq!(second.turns.len(), 4);
```

The highest-value assertion you can write about an agent is that its **tool block is byte-identical
between turns**, because that block is the stable cache prefix and nothing else will tell you when
it stops being stable. No provider reports a cache miss. The only symptom is the bill.

```rust
let first = model.saw()[0].tools.clone();
let later = model.saw()[2].tools.clone();
assert_eq!(first, later, "the tool block must not churn between turns");
```

`ScriptedModel::with_capabilities` lets you test against a model's *declared* limits — a small
context window, a large minimum cacheable prefix — without paying for that model.

## Hostile toolsets

`FakeToolset` takes a `Hostility` so you can test the paths that only happen when something else
misbehaves:

```rust
let toolset = FakeToolset::new("github", tools).hostile(Hostility { reorder_listings: true, .. });
assert_eq!(toolset.listing_count(), 1, "definitions are fetched once per step, not per call");
```

A server that reorders its listing every request would churn your prompt cache. Frey's client
re-sorts defensively; this is how you check that your own code does not undo that.

## Audit assertions

`CapturedAudit` records every integrity change, so you can assert the property an auditor will ask
about:

```rust
assert!(audit.endorsements().is_empty(), "nothing in this path raises integrity");
```

## What to test, in order of value

1. **The tool block is stable across turns.** Everything about cost follows from this.
2. **A tool failure carries guidance the model can act on.** Assert the guidance text, not just that
   an error occurred — the guidance is the part that changes behaviour.
3. **Denials do not end the run.** A refused tool call should produce a different next turn, not a
   crash.
4. **Untrusted data stays untrusted.** Mostly the compiler's job; the interesting case is where you
   `endorse`, and that should be a named test.
5. **Your MCP surface**, if you have one. thicket's
   [`tests/mcp.rs`](https://github.com/newsbubbles/thicket/blob/main/tests/mcp.rs) is a template.

## Live tests

Keep them separate and off by default. `cargo run -p frey --example live_openrouter -- <model>` in
this repo is the pattern: gated on a key being present, exits with a clear message when it is not,
and asserts a **verifiable** answer rather than an impressive-looking one.

Choose that task carefully. The obvious ones secretly test mental arithmetic — three of five models
tested fetched every value correctly and then added them up wrong, which looks like a tool-calling
failure and is not.

## Replay

The journal records every non-deterministic effect, so a recorded run replays exactly:

```rust
let replay = Replay::new(run.journal.clone());
```

Replay **diverges loudly at the first mismatch**, naming what was recorded and what the run
produced. A replay that quietly adapts is worse than none, because it produces confident results
about a run that never happened.

One documented limit: the fingerprint compares request *shape* — model, turn count, tool names —
not full prompt text, because a journal storing every prompt verbatim would be enormous. The test
that pins this also documents what it therefore cannot catch.
