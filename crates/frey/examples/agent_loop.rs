//! A whole agent, end to end, with no API key.
//!
//! Run with `cargo run -p frey --example agent_loop`.
//!
//! Uses the scripted model from `frey-testkit`, which is how Frey's own tests exercise the loop —
//! and which is also how you should test an agent you build on it. Everything except the provider
//! is the real thing: the context plan, the tool layers, the journal, the ledger.

use frey::prelude::*;
use frey_core::item::{Caller, ToolCallItem};
use frey_core::taint::Provenance;
use frey_testkit::scripted::{ScriptedModel, Turn as Scripted};

/// A toolset with one tool that reads a file.
struct Workspace;

impl ToolHost for Workspace {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents as text",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."}
                },
                "required": ["path"]
            }))
            .unwrap(),
        )]
    }

    async fn call(
        &self,
        invocation: Invocation,
        _cx: &ToolCx,
    ) -> ToolOutcome<frey_core::tool::ToolValue> {
        let path = invocation.args.get("path").and_then(|p| p.as_str()).unwrap_or_default();
        if path.contains("..") {
            // A refusal the model can act on. A bare "denied" produces a retry with the same
            // arguments, which is worse than useless.
            return ToolOutcome::Denied(
                ToolError::new(ToolErrorKind::Denied, "that path leaves the workspace")
                    .guide("Use a path relative to the workspace root, with no `..` components."),
            );
        }
        ToolOutcome::Ok(Tainted::with_provenance(
            ToolContent::text(format!("// contents of {path}\nfn main() {{}}\n")),
            Provenance::new("tool:fs_read"),
        ))
    }
}

fn tool_call(path: &str) -> Item {
    Item::ToolCall(ToolCallItem {
        id: CallId::new("call-1"),
        name: ToolName::new("fs_read"),
        args: serde_json::json!({"path": path}),
        caller: Caller::Direct,
    })
}

fn main() {
    // Two turns: the model asks for a file, then answers.
    let model = ScriptedModel::new(vec![
        Scripted::tool_calls(vec![tool_call("src/main.rs")]),
        Scripted::text("It is an empty main function."),
    ]);

    let agent = Agent::new(model.clone(), Workspace, "scripted-model")
        .system("You are a careful assistant. Read before you answer.")
        .max_turns(6);

    let run = pollster::block_on(agent.run("What is in src/main.rs?")).expect("the run completes");

    println!("answer: {}", run.text());
    println!("\njournal ({} recorded effects):", run.journal.len());
    for entry in &run.journal.entries {
        println!("  {}  {}", entry.seq, entry.effect.label());
    }

    println!("\nwhat the model was shown on the second turn:");
    let second = &model.saw()[1];
    println!("  tools visible: {:?}", second.tools.iter().map(|t| &t.name).collect::<Vec<_>>());
    println!("  turns sent:    {}", second.turns.len());

    println!("\nusage:");
    for (model_key, totals) in &run.totals.by_model {
        println!("  {model_key}: {} calls", totals.calls);
    }
    // The scripted model reports no cost, and Frey does not invent one.
    println!("  reported cost: {:?}", run.cost);
    println!("  complete:      {}", run.totals.is_complete());

    println!("\nwarnings: {}", run.warnings.len());
    for warning in &run.warnings {
        println!("  {warning:?}");
    }

    // A denial reaches the model with guidance rather than ending the run.
    println!("\n== a refused path ==");
    let refusing = ScriptedModel::new(vec![
        Scripted::tool_calls(vec![tool_call("../../etc/passwd")]),
        Scripted::text("I cannot read outside the workspace, so I stopped."),
    ]);
    let recovered = pollster::block_on(
        Agent::new(refusing, Workspace, "scripted-model").max_turns(4).run("read /etc/passwd"),
    )
    .expect("a denial does not end the run");
    println!("answer: {}", recovered.text());
}
