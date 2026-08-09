# Getting started

## Install

Frey is not on crates.io yet. Depend on it by git:

```toml
[dependencies]
frey = { git = "https://github.com/newsbubbles/frey" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
serde_json = "1"
```

Rust 1.94 or later, edition 2024.

Default features are `http`, `mcp`, `sandbox`, `harness`. Take what you need:

```toml
frey = { git = "…", default-features = false, features = ["mcp", "http"] }
```

| Feature | What it adds |
|---|---|
| `http` | The real HTTP transport. Without it the wire mapping remains and is testable offline. |
| `mcp` | Model Context Protocol — client *and* server. |
| `sandbox` | Cross-platform process confinement. |
| `harness` | Sessions, approvals, AG-UI, `doctor`. |
| `a2a` | Agent-to-agent interoperability. |
| `agent-cli` | Delegate to a vendor's own CLI so a user rides their subscription. |
| `full` | All of the above. |

## One import

```rust
use frey::prelude::*;
```

The prelude is curated rather than a set of glob re-exports, because three names genuinely collide
across crates. `Request` means one thing to a provider and another to MCP, `ApprovalPolicy` exists
at both the tool and the harness layer, and `validate` is a verb two subsystems need. The colliding
names are aliased — `ModelRequest`, `ToolApprovalPolicy`, `SessionApprovalPolicy`, `validate_exec`,
`validate_harness` — so both stay reachable and neither is a surprise.

**One path worth knowing before you need it.** The sub-crates are re-exported through the facade, so
it is `frey::core::taint::Provenance`, not `frey_core::taint::Provenance`. The prelude covers the
common cases; when it does not, that is the path.

## A first agent

```rust
use std::sync::Arc;
use frey::prelude::*;
use frey::core::tool::ToolValue;

struct Clock;

impl ToolHost for Clock {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::new(
            "days_until",
            "Days between today and a date.",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {
                    "date": {"type": "string", "description": "Target date as YYYY-MM-DD."}
                },
                "required": ["date"],
                "additionalProperties": false
            })).unwrap(),
        )]
    }

    async fn call(&self, invocation: Invocation, cx: &ToolCx) -> ToolOutcome<ToolValue> {
        let date = invocation.args["date"].as_str().unwrap_or_default();
        ToolOutcome::Ok(Tainted::with_provenance(
            ToolContent::text(format!("42 days until {date}")),
            cx.provenance.clone(),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = HttpProvider::new(
        Arc::new(OpenRouter),
        "https://openrouter.ai/api/v1",
        Auth::Bearer { env: "OPENROUTER_API_KEY".into() },
    )?;

    let run = Agent::new(provider, Clock, "qwen/qwen3-30b-a3b-instruct-2507")
        .system("You are precise. Use tools rather than estimating.")
        .max_turns(10)
        .run("How long until 2027-01-01?")
        .await?;

    println!("{}", run.text());
    println!("cost: {:?}", run.cost);
    for warning in &run.warnings {
        println!("warning: {warning:?}");
    }
    Ok(())
}
```

Three things about that code are deliberate and worth noticing.

**The key is named, not passed.** `Auth::Bearer { env: … }` reads the variable at request time. A
credential never enters your configuration, your struct, or a log line.

**`args["date"]` is safe to index without checking the type.** Frey validates arguments against the
declared schema before your tool runs. A model that sends a number where you asked for a string gets
an error naming the field, and your code is never reached.

**Warnings are not decoration.** They are how the framework tells you it is doing something less
useful than you asked — a cache that is not caching, a budget under pressure, a turn that asked for
more work than it is allowed. Print them during development at minimum.

## Tokio is required

`HttpProvider` is built on `reqwest`, which is built on Tokio. `pollster::block_on` works for the
scripted model and will not work for the network.

## Which model

Any. Frey is not opinionated, but tool-calling quality varies enormously and the failure modes are
worth knowing:

- **Small models emit floods of tool calls.** One 8B model produced ~145 in a single response during
  testing. Frey caps this at 32 per turn by default and refuses the excess with an error the model
  can act on; tune with `.max_tool_calls_per_turn(n)`.
- **Arithmetic is where models fail, not tool use.** Three of five models tested fetched every value
  correctly and then added them up wrong. If your task involves numbers, give the agent a calculator
  and do not conclude that tool calling is broken.

## Check the host

```bash
cargo run -p frey-cli -- doctor
```

Reports what confinement is actually available, which providers are configured, and what is
degraded. `--json` output is treated as a stable API and pinned by a test, because a coding agent
parses it to orient in an unfamiliar project.

## Next

- [Tools](tools.md) — writing what your agent can do, and errors it can recover from.
- [Providers](providers.md) — the differences that cost money.
- [Context and caching](context-and-caching.md) — the part that is actually novel.
