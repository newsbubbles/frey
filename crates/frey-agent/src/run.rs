//! The agent loop.
//!
//! Model, tools, context, cost, and a journal — the smallest thing that is actually an agent.
//!
//! Each turn is the same five steps: build segments from what the model will see, budget and plan
//! the cache against those segments, call the provider, record what came back, then run whatever
//! tools it asked for through the tool layers. Nothing about that order is negotiable: the cache
//! plan has to see the final segment list, and the tool layers have to see the call before anything
//! executes.

use frey_context::budget::{Budgeter, ContextBudget};
use frey_context::cache::{CachePlanner, PreviousPrompt, check_lookback};
use frey_context::hash::hash_parts;
use frey_core::error::{ToolError, ToolErrorKind, ToolOutcome};
use frey_core::event::{Event, EventKind, Warning};
use frey_core::ids::{RunId, SegmentId, SeqId, SessionId};
use frey_core::item::{Item, Role, ToolResultItem, Turn};
use frey_core::provider::{ModelProvider, ProviderError, Request, StopReason};
use frey_core::segment::{Segment, SegmentKind, Stability};
use frey_core::tool::{Invocation, ToolCx};
use frey_core::tool_def::ToolDefinition;
use frey_core::usage::{CostEstimate, UsageTotals};
use frey_tools::layer::PolicyLayer;

use crate::journal::{Effect, Journal, effect_of};

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// The provider failed in a way that ends the run.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The prompt could not be made to fit.
    #[error(transparent)]
    Budget(#[from] frey_context::budget::DoesNotFit),
    /// The loop hit its turn limit.
    #[error(
        "the agent used all {limit} turns without finishing. Raise max_turns, or look at the \
         transcript for a loop: repeating the same failing tool call is the usual cause."
    )]
    TurnLimit {
        /// The limit that was hit.
        limit: u32,
    },
    /// The run needs something from outside it and nothing was there to supply it.
    #[error("the run needs input ({what}) and no approval handler was configured")]
    NeedsInput {
        /// What was wanted.
        what: String,
    },
}

/// What a finished run produced.
#[derive(Debug, Clone)]
pub struct RunOutput {
    /// The final assistant turn.
    pub items: Vec<Item>,
    /// Everything the run consumed.
    pub totals: UsageTotals,
    /// What it cost, when that can be said without inventing a number.
    pub cost: Option<CostEstimate>,
    /// The full record.
    pub journal: Journal,
    /// Diagnostics worth surfacing.
    pub warnings: Vec<Warning>,
}

impl RunOutput {
    /// The assistant's text, concatenated.
    #[must_use]
    pub fn text(&self) -> String {
        self.items
            .iter()
            .filter_map(|i| match i {
                Item::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Something the loop can call.
pub trait ToolHost: Send + Sync {
    /// The tools visible this step, in presentation order.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Run one.
    fn call(
        &self,
        invocation: Invocation,
        cx: &ToolCx,
    ) -> impl Future<Output = ToolOutcome<frey_core::tool::ToolValue>> + Send;
}

/// An agent: a model, a system prompt, some tools, and a budget.
#[derive(Debug, Clone)]
pub struct Agent<P, T> {
    provider: P,
    tools: T,
    model: frey_core::ids::ModelId,
    system: Option<String>,
    max_turns: u32,
    session: SessionId,
}

impl<P: ModelProvider, T: ToolHost> Agent<P, T> {
    /// An agent using `provider` and `tools`.
    pub fn new(provider: P, tools: T, model: impl Into<frey_core::ids::ModelId>) -> Self {
        Self {
            provider,
            tools,
            model: model.into(),
            system: None,
            max_turns: 24,
            session: SessionId::new("default"),
        }
    }

    /// Set the system prompt.
    #[must_use]
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Cap how many model calls one run may make.
    #[must_use]
    pub fn max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Name the session, so a journal can be resumed against it.
    #[must_use]
    pub fn session(mut self, session: SessionId) -> Self {
        self.session = session;
        self
    }

    /// Run `task` to completion.
    ///
    /// # Errors
    /// Returns [`RunError`]. A fatal provider failure — auth or billing — ends the run rather than
    /// being retried, so a run never degrades into producing nothing while still being billed.
    pub async fn run(&self, task: impl Into<String>) -> Result<RunOutput, RunError> {
        let run_id = RunId::new(format!("{}-run", self.session));
        let mut journal = Journal::new(run_id.clone());
        let mut warnings = Vec::new();
        let mut totals = UsageTotals::default();
        let mut previous = PreviousPrompt::none();

        let mut turns: Vec<Turn> = Vec::new();
        if let Some(system) = &self.system {
            turns.push(Turn::system(system.clone()));
        }
        turns.push(Turn::user(task));

        journal.record_event(Event::root(SeqId::FIRST, EventKind::RunStarted { run: run_id }));

        let caps = self.provider.capabilities(&self.model);
        let budget = ContextBudget::from_capabilities(&caps);
        let definitions = self.tools.definitions();

        for turn_index in 0..self.max_turns {
            let span = tracing::info_span!(
                "frey.turn",
                turn = turn_index,
                model = %self.model,
                "gen_ai.system" = %self.provider.id(),
            );
            let _guard = span.enter();

            // 1. Segment the prompt, so budgeting and cache planning have something to reason over.
            let segments = build_segments(&definitions, &turns);

            // 2. Fit it. Eviction is never silent.
            let fitted = Budgeter::fit(&segments, &budget)?;
            warnings.extend(fitted.warnings.iter().cloned());

            // 3. Plan the cache against the segments that survived.
            let plan = CachePlanner::plan(&fitted.keep, &previous, &caps);
            warnings.extend(plan.warnings.iter().cloned());
            for warning in &plan.warnings {
                journal.record_event(Event::root(
                    SeqId(turn_index),
                    EventKind::Warned { warning: warning.clone() },
                ));
            }
            previous = PreviousPrompt::from_segments(&fitted.keep);

            // 4. Ask the model.
            let request = Request {
                model: self.model.clone(),
                turns: turns.clone(),
                tools: definitions.clone(),
                marks: plan.marks.clone(),
                max_output: caps.max_output.min(budget.reserve_output),
                cache_key: Some(self.session.as_str().into()),
                ..Request::default()
            };

            let response = self.provider.complete(request.clone()).await?;
            journal.record(effect_of(&request, &response));
            totals
                .record(&format!("{}:{}", response.provider, response.model), &response.usage)
                .unwrap_or_else(|_| {
                    warnings.push(Warning::Degraded {
                        capability: "cost-accounting".into(),
                        fallback: "a call reported a different currency; totals are partial".into(),
                    });
                });
            journal.record_event(Event::root(
                SeqId(turn_index),
                EventKind::UsageUpdated { usage: response.usage.clone() },
            ));

            if response.stop.is_truncated() {
                warnings.push(Warning::Degraded {
                    capability: "output-length".into(),
                    fallback: "the model hit its output cap; the answer is incomplete".into(),
                });
            }

            let calls: Vec<_> = response
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::ToolCall(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();

            // A long agentic turn can push the previous breakpoint further back than the provider
            // searches, which misses the cache with no error from anyone.
            let blocks_added =
                u32::try_from(response.items.len() + calls.len()).unwrap_or(u32::MAX);
            if let Some(warning) = check_lookback(blocks_added) {
                warnings.push(warning);
            }

            turns.push(Turn::new(Role::Assistant, response.items.clone()));

            if calls.is_empty() || response.stop == StopReason::EndTurn {
                journal.record_event(Event::root(
                    SeqId(turn_index),
                    EventKind::RunFinished { totals: totals.clone(), cost: None },
                ));
                // The same warning on every turn is noise that trains people to ignore warnings.
                // Each distinct one is reported once, in the order it first appeared.
                warnings.dedup();
                return Ok(RunOutput {
                    items: response.items,
                    cost: totals.reported_cost.map(|amount| CostEstimate {
                        amount,
                        source: frey_core::usage::PricingSource::Reported,
                    }),
                    totals,
                    journal,
                    warnings,
                });
            }

            // 5. Run the tools the model asked for, through the layers.
            let mut results = Vec::new();
            for call in calls {
                let cx = ToolCx {
                    run: journal.run.clone(),
                    session: self.session.clone(),
                    grants: frey_core::capability::GrantSet::empty(),
                    provenance: frey_core::taint::Provenance::new(format!("tool:{}", call.name)),
                };
                let invocation = Invocation {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.args.clone(),
                    caller: call.caller.clone(),
                };

                let definition = definitions.iter().find(|d| d.name == call.name);
                let outcome = match definition {
                    None => ToolOutcome::Failed(
                        ToolError::new(
                            ToolErrorKind::NotFound,
                            format!("there is no tool called `{}`", call.name),
                        )
                        .guide("Use one of the tools that were listed, or search for one."),
                    ),
                    Some(def) => match PolicyLayer::check(def, &invocation, &cx) {
                        Some(denied) => ToolOutcome::Denied(denied),
                        None => self.tools.call(invocation, &cx).await,
                    },
                };

                let (content, is_error, elided) = render_outcome(&outcome);
                journal.record(Effect::ToolResult {
                    tool: call.name.as_str().into(),
                    content: content.clone(),
                    is_error,
                });
                results.push(Item::ToolResult(ToolResultItem {
                    id: call.id.clone(),
                    content,
                    is_error,
                    bytes_elided: elided,
                    provenance: cx.provenance.clone(),
                }));
            }
            turns.push(Turn::new(Role::User, results));
        }

        Err(RunError::TurnLimit { limit: self.max_turns })
    }
}

/// Render a tool outcome for the model, keeping the truncation count so it is never silent.
fn render_outcome(outcome: &ToolOutcome<frey_core::tool::ToolValue>) -> (String, bool, u64) {
    match outcome {
        ToolOutcome::Ok(value) => {
            let content = value.peek();
            let mut text = content.text.clone();
            if content.bytes_elided > 0 {
                // The model is told how much it cannot see, so it can decide whether to narrow the
                // request rather than reasoning over a silently partial answer.
                text.push_str(&format!(
                    "\n\n[{} more bytes were withheld. Narrow the request to see them.]",
                    content.bytes_elided
                ));
            }
            (text, false, content.bytes_elided)
        }
        ToolOutcome::Failed(e) | ToolOutcome::Denied(e) => {
            let mut text = e.model().summary.clone();
            if let Some(guidance) = &e.model().guidance {
                text.push(' ');
                text.push_str(guidance);
            }
            (text, true, 0)
        }
        // `ToolOutcome` is non-exhaustive, and an unknown outcome must reach the model as a
        // failure rather than as silence: an empty result would read as "the tool did nothing".
        _ => ("This action needs approval that was not available.".to_string(), true, 0),
    }
}

/// Split what the model will see into segments the planner can reason about.
///
/// Order is the prefix hierarchy: tools, then system, then history. A change at one level
/// invalidates that level and everything after it, so putting them in any other order would make
/// the cache plan wrong.
fn build_segments(definitions: &[ToolDefinition], turns: &[Turn]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut next_id = 0u32;

    if !definitions.is_empty() {
        let parts: Vec<String> = definitions
            .iter()
            .map(|d| format!("{}|{}|{}", d.name, d.description, d.input_schema.as_value()))
            .collect();
        let est: u32 = parts.iter().map(|p| estimate_tokens(p)).sum();
        segments.push(Segment {
            id: SegmentId(next_id),
            kind: SegmentKind::Tools,
            stability: Stability::Static,
            hash: hash_parts(parts.iter().map(String::as_str)),
            est_tokens: est,
            label: "tools".into(),
        });
        next_id += 1;
    }

    for turn in turns {
        let text: String = turn
            .items
            .iter()
            .map(|i| match i {
                Item::Text(t) => t.text.clone(),
                Item::ToolCall(c) => format!("{}{}", c.name, c.args),
                Item::ToolResult(r) => r.content.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n");

        let (kind, stability, label) = match turn.role {
            Role::System => (SegmentKind::System, Stability::Static, "system"),
            _ => (SegmentKind::History, Stability::Volatile, "history"),
        };

        segments.push(Segment {
            id: SegmentId(next_id),
            kind,
            stability,
            hash: hash_parts([text.as_str()]),
            est_tokens: estimate_tokens(&text),
            label: label.into(),
        });
        next_id += 1;
    }

    segments
}

/// A rough token count.
///
/// Deliberately crude and deliberately documented as such: it is used for budgeting and for the
/// minimum-cacheable-prefix check, both of which need an order of magnitude rather than a
/// tokenizer. A real tokenizer would be per-model, would have to be shipped or downloaded, and
/// would make this crate impure.
fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(4)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::ids::{CallId, ToolName};
    use frey_core::item::{Caller, ToolCallItem};
    use frey_core::taint::Tainted;
    use frey_core::tool::{ToolContent, ToolValue};
    use frey_core::tool_def::JsonSchema;
    use frey_testkit::scripted::{ScriptedModel, Turn as Scripted};

    struct Tools {
        definitions: Vec<ToolDefinition>,
        reply: String,
    }

    impl ToolHost for Tools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.definitions.clone()
        }

        async fn call(&self, _invocation: Invocation, cx: &ToolCx) -> ToolOutcome<ToolValue> {
            ToolOutcome::Ok(Tainted::with_provenance(
                ToolContent::text(self.reply.clone()),
                cx.provenance.clone(),
            ))
        }
    }

    fn tools(names: &[&str]) -> Tools {
        Tools {
            definitions: names
                .iter()
                .map(|n| {
                    ToolDefinition::new(
                        *n,
                        "A tool described well enough to be found by a search",
                        JsonSchema::empty_object(),
                    )
                })
                .collect(),
            reply: "tool output".into(),
        }
    }

    fn tool_call(name: &str) -> Item {
        Item::ToolCall(ToolCallItem {
            id: CallId::new("c1"),
            name: ToolName::new(name),
            args: serde_json::json!({}),
            caller: Caller::Direct,
        })
    }

    #[test]
    fn a_plain_question_takes_one_turn() {
        let model = ScriptedModel::replying("the answer");
        let agent = Agent::new(model.clone(), tools(&[]), "test-model");
        let out = pollster::block_on(agent.run("what is 2+2?")).unwrap();

        assert_eq!(out.text(), "the answer");
        assert_eq!(model.call_count(), 1);
        assert_eq!(out.journal.len(), 1, "one recorded model response");
    }

    #[test]
    fn a_tool_call_round_trips_and_is_journalled() {
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(vec![tool_call("fs_read")]),
            Scripted::text("done"),
        ]);
        let agent = Agent::new(model.clone(), tools(&["fs_read"]), "test-model");
        let out = pollster::block_on(agent.run("read a file")).unwrap();

        assert_eq!(out.text(), "done");
        assert_eq!(model.call_count(), 2);
        assert_eq!(out.journal.len(), 3, "two model calls and one tool result");
        assert!(matches!(out.journal.entries[1].effect, Effect::ToolResult { .. }));
    }

    #[test]
    fn the_model_sees_the_tool_result_on_the_next_turn() {
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(vec![tool_call("fs_read")]),
            Scripted::text("done"),
        ]);
        let agent = Agent::new(model.clone(), tools(&["fs_read"]), "test-model");
        pollster::block_on(agent.run("read a file")).unwrap();

        let second = model.saw()[1].clone();
        let has_result = second
            .turns
            .iter()
            .flat_map(|t| &t.items)
            .any(|i| matches!(i, Item::ToolResult(r) if r.content == "tool output"));
        assert!(has_result, "the loop must feed results back or the model repeats itself");
    }

    #[test]
    fn calling_a_tool_that_does_not_exist_tells_the_model_what_to_do() {
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(vec![tool_call("does_not_exist")]),
            Scripted::text("recovered"),
        ]);
        let agent = Agent::new(model.clone(), tools(&["fs_read"]), "test-model");
        let out = pollster::block_on(agent.run("go")).unwrap();

        assert_eq!(out.text(), "recovered");
        let Effect::ToolResult { content, is_error, .. } = &out.journal.entries[1].effect else {
            panic!("expected a tool result")
        };
        assert!(is_error);
        assert!(content.contains("search for one"), "a bare failure just loops: {content}");
    }

    #[test]
    fn a_fatal_provider_failure_ends_the_run_rather_than_retrying() {
        // The failure this classification exists for: a run that degrades into silent no-ops while
        // still being billed is worse than one that stops.
        let model = ScriptedModel::new(vec![Scripted::Fail(ProviderError::Billing {
            provider: frey_core::ids::ProviderId::new("scripted"),
            detail: "out of credit".into(),
        })]);
        let agent = Agent::new(model, tools(&[]), "test-model");
        let err = pollster::block_on(agent.run("go")).unwrap_err();
        assert!(matches!(err, RunError::Provider(e) if e.is_fatal()));
    }

    #[test]
    fn an_endless_loop_stops_with_advice_rather_than_running_forever() {
        let model = ScriptedModel::new(
            (0..4).map(|_| Scripted::tool_calls(vec![tool_call("fs_read")])).collect(),
        );
        let agent = Agent::new(model, tools(&["fs_read"]), "test-model").max_turns(3);
        let err = pollster::block_on(agent.run("go")).unwrap_err();
        assert!(matches!(err, RunError::TurnLimit { limit: 3 }));
        assert!(format!("{err}").contains("repeating the same failing tool call"));
    }

    #[test]
    fn the_tool_block_is_the_first_segment_and_stays_stable() {
        let segments = build_segments(
            &tools(&["fs_read", "shell"]).definitions,
            &[Turn::system("be careful"), Turn::user("hello")],
        );
        assert_eq!(segments[0].kind, SegmentKind::Tools);
        assert_eq!(segments[0].stability, Stability::Static);
        assert_eq!(segments[1].kind, SegmentKind::System);
        assert_eq!(segments[2].stability, Stability::Volatile, "history churns by definition");
    }

    #[test]
    fn a_stable_prompt_produces_no_cache_warnings_across_turns() {
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(vec![tool_call("fs_read")]),
            Scripted::text("done"),
        ]);
        let agent = Agent::new(model, tools(&["fs_read"]), "test-model").system("be careful");
        let out = pollster::block_on(agent.run("go")).unwrap();

        assert!(
            !out.warnings.iter().any(|w| matches!(w, Warning::CacheChurn { .. })),
            "a stable tool block and system prompt must not report churn: {:?}",
            out.warnings
        );
    }

    #[test]
    fn usage_is_tracked_per_model_and_cost_is_not_invented() {
        let model = ScriptedModel::replying("done");
        let agent = Agent::new(model, tools(&[]), "test-model");
        let out = pollster::block_on(agent.run("go")).unwrap();

        assert_eq!(out.totals.by_model.len(), 1);
        assert_eq!(out.cost, None, "the scripted provider reports no cost, so neither do we");
        assert!(!out.totals.is_complete(), "and the ledger says it is incomplete");
    }

    #[test]
    fn token_estimation_is_monotonic_even_though_it_is_crude() {
        assert!(estimate_tokens("a longer piece of text") > estimate_tokens("short"));
        assert_eq!(estimate_tokens(""), 0);
    }
}
