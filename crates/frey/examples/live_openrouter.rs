//! A real agent, against a real provider, with real tool calls.
//!
//! ```text
//! OPENROUTER_API_KEY=... cargo run -p frey --example live_openrouter -- <model-id>
//! ```
//!
//! Everything else in Frey's test suite runs against the scripted model, which proves the loop is
//! correct but cannot prove the *wire mapping* is. This example is the other half: it spends real
//! tokens to find out whether a model actually receives the tools we think we sent it.
//!
//! The task is chosen so the answer can be checked rather than admired. Three stations hold three
//! numbers; the model has to discover the station list, fetch each reading, and add them up. The
//! sum is 1101, and a model that hallucinates instead of calling the tools will not produce it.

use std::sync::Arc;

use frey::prelude::*;
use frey_core::taint::Provenance;
use frey_core::tool::ToolValue;

/// The readings. `station_list` must be called to learn the names.
const READINGS: &[(&str, i64)] = &[("alpha", 412), ("bravo", 377), ("cygnus", 312)];

/// The sum a correct run reports.
const EXPECTED_TOTAL: i64 = 1101;

struct Field;

impl ToolHost for Field {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>, ToolError> {
        Ok(vec![
            ToolDefinition::new(
                "station_list",
                "List the names of every field station reporting readings.",
                JsonSchema::new(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }))
                .unwrap(),
            ),
            ToolDefinition::new(
                "fetch_reading",
                "Fetch the latest sensor reading for one field station.",
                JsonSchema::new(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "station": {
                            "type": "string",
                            "description": "The station name, exactly as returned by station_list."
                        }
                    },
                    "required": ["station"],
                    "additionalProperties": false
                }))
                .unwrap(),
            ),
            ToolDefinition::new(
                "calibrate",
                "Apply a calibration offset to a station. Offsets outside -10..=10 are rejected.",
                JsonSchema::new(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "station": {"type": "string", "description": "The station to calibrate."},
                        "offset": {"type": "integer", "description": "Offset to apply, -10 to 10."}
                    },
                    "required": ["station", "offset"],
                    "additionalProperties": false
                }))
                .unwrap(),
            ),
        ])
    }

    async fn call(&self, invocation: Invocation, _cx: &ToolCx) -> ToolOutcome<ToolValue> {
        let provenance = Provenance::new(format!("tool:{}", invocation.name));
        let ok = |text: String| {
            ToolOutcome::Ok(Tainted::with_provenance(ToolContent::text(text), provenance.clone()))
        };

        match invocation.name.as_str() {
            "station_list" => {
                let names: Vec<&str> = READINGS.iter().map(|(n, _)| *n).collect();
                ok(names.join(", "))
            }
            "fetch_reading" => {
                let Some(station) = invocation.args.get("station").and_then(|s| s.as_str()) else {
                    // A refusal the model can act on. A bare "bad arguments" produces a retry with
                    // the same arguments, which is worse than useless.
                    return ToolOutcome::Failed(
                        ToolError::new(ToolErrorKind::InvalidArgs, "`station` is required")
                            .guide("Call station_list first, then pass one name as `station`."),
                    );
                };
                match READINGS.iter().find(|(n, _)| *n == station) {
                    Some((_, value)) => ok(format!("{station}: {value}")),
                    None => ToolOutcome::Failed(
                        ToolError::new(
                            ToolErrorKind::InvalidArgs,
                            format!("no station named `{station}`"),
                        )
                        .guide("Call station_list to get the exact names, then try again."),
                    ),
                }
            }
            "calibrate" => {
                let offset = invocation.args.get("offset").and_then(serde_json::Value::as_i64);
                match offset {
                    Some(o) if (-10..=10).contains(&o) => ok(format!("calibrated by {o}")),
                    Some(o) => ToolOutcome::Denied(
                        ToolError::new(
                            ToolErrorKind::Denied,
                            format!("offset {o} is outside the permitted range"),
                        )
                        .guide("Offsets must be between -10 and 10. Pick one in range, or skip calibration — it is not needed to report readings."),
                    ),
                    None => ToolOutcome::Failed(
                        ToolError::new(ToolErrorKind::InvalidArgs, "`offset` must be an integer")
                            .guide("Pass `offset` as a whole number between -10 and 10."),
                    ),
                }
            }
            other => ToolOutcome::Failed(
                ToolError::new(ToolErrorKind::NotFound, format!("no tool named `{other}`"))
                    .guide("Use one of: station_list, fetch_reading, calibrate."),
            ),
        }
    }
}

#[tokio::main]
async fn main() {
    let model =
        std::env::args().nth(1).unwrap_or_else(|| "qwen/qwen3-30b-a3b-instruct-2507".into());

    if std::env::var("OPENROUTER_API_KEY").is_err() {
        // Say why rather than doing nothing. A silent no-op here reads as a pass.
        eprintln!(
            "OPENROUTER_API_KEY is not set — this example needs a real key and spends money."
        );
        std::process::exit(2);
    }

    let provider = HttpProvider::new(
        Arc::new(OpenRouter),
        "https://openrouter.ai/api/v1",
        Auth::Bearer { env: "OPENROUTER_API_KEY".into() },
    )
    .expect("the HTTP client builds");

    let agent = Agent::new(provider, Field, model.clone())
        .system(
            "You are a field data assistant. Use the tools to get real numbers; never guess a \
             reading. When you have all readings, reply with the total as a plain number.",
        )
        .max_turns(10);

    println!("== {model} ==");
    let started = std::time::Instant::now();
    let run =
        match agent.run("What is the sum of the latest readings from every field station?").await {
            Ok(run) => run,
            Err(e) => {
                println!("  RUN FAILED: {e}");
                std::process::exit(1);
            }
        };
    let elapsed = started.elapsed();

    let answer = run.text();
    let correct = answer.contains(&EXPECTED_TOTAL.to_string());
    println!("  answer:   {}", answer.trim().replace('\n', " / "));
    println!("  correct:  {correct}  (expected {EXPECTED_TOTAL})");
    println!("  elapsed:  {:.1}s", elapsed.as_secs_f64());

    let calls: Vec<_> = run.journal.entries.iter().map(|e| e.effect.label().to_string()).collect();
    println!("  journal:  {} effects: {}", calls.len(), calls.join(" → "));

    for (key, totals) in &run.totals.by_model {
        println!(
            "  usage:    {key}: {} calls, {} in, {} out, {} cached",
            totals.calls, totals.input, totals.output, totals.cache_read
        );
    }
    println!("  complete: {}", run.totals.is_complete());
    match &run.cost {
        Some(cost) => println!("  cost:     {cost:?}"),
        None => println!("  cost:     not reported"),
    }

    if run.warnings.is_empty() {
        println!("  warnings: none");
    } else {
        for warning in &run.warnings {
            println!("  warning:  {warning:?}");
        }
    }
}
