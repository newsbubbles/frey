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
use frey_tools::validate::check_arguments;

use crate::journal::{Effect, Journal, effect_of};

/// How many tool calls one model response may have executed, unless the agent says otherwise.
///
/// Chosen to sit above what a competent model asks for and far below a runaway. Well-behaved
/// parallel fan-out in the wild is single digits; the pathological case measured during the first
/// live session was about 145 calls in one response from an 8B model that had lost the thread.
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: u32 = 32;

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
    ///
    /// Carries the journal, because this error's own advice is to read the transcript and there was
    /// previously no way to: `run` returned `Err` and the record went out of scope with it. A run
    /// that loops is the case where the transcript matters most and it was the one case that threw
    /// it away.
    #[error(
        "the agent used all {limit} turns without finishing. Raise max_turns, or look at the \
         transcript for a loop: repeating the same failing tool call is the usual cause."
    )]
    TurnLimit {
        /// The limit that was hit.
        limit: u32,
        /// What happened, up to the limit.
        journal: Box<Journal>,
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
    /// Why the final turn stopped.
    ///
    /// The difference between an answer and *most* of an answer. A run whose last turn hit the
    /// output cap returns `Ok` with real content, because throwing that away would be worse — but
    /// it is not a finished answer, and a caller has to be able to tell without reading prose.
    /// See [`is_complete`](Self::is_complete).
    pub stop: StopReason,
}

impl RunOutput {
    /// Whether the run reached a natural end rather than a limit.
    ///
    /// `false` means the final turn was cut off — the model hit its output cap mid-sentence, and
    /// [`text`](Self::text) is a prefix of what it meant to say. Common rather than exotic with
    /// reasoning models, which can spend the whole budget thinking before they write anything.
    ///
    /// This exists because the alternative was asking callers to scan `warnings` for a
    /// [`Warning::Degraded`] with a particular string in it. Frey's rule is that nothing degrades
    /// quietly, and a diagnostic you have to grep for is quiet enough — the same reasoning that
    /// makes `cost` an `Option` rather than a zero, and `UsageTotals::is_complete` a method rather
    /// than a comment.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.stop.is_truncated()
    }

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
    max_tool_calls_per_turn: u32,
    session: SessionId,
    cache_key: Option<smol_str::SmolStr>,
    extra: std::collections::BTreeMap<smol_str::SmolStr, serde_json::Value>,
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
            max_tool_calls_per_turn: DEFAULT_MAX_TOOL_CALLS_PER_TURN,
            session: SessionId::new("default"),
            cache_key: None,
            extra: std::collections::BTreeMap::new(),
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

    /// Cap how many tool calls a *single* model response may have executed.
    ///
    /// This is a different limit from [`max_turns`](Self::max_turns) and neither implies the other.
    /// A turn limit bounds how many times the model is consulted; this bounds how much work one
    /// answer can demand. Raise it for an agent that legitimately fans out — a parallel file read
    /// over a large tree is well-behaved — and lower it when every tool call has a side effect.
    ///
    /// Excess calls are refused individually, with an error the model can act on, rather than
    /// dropped. A silently discarded call looks to the model exactly like one that succeeded and
    /// returned nothing.
    #[must_use]
    pub fn max_tool_calls_per_turn(mut self, max: u32) -> Self {
        self.max_tool_calls_per_turn = max;
        self
    }

    /// Name the session, so a journal can be resumed against it.
    #[must_use]
    pub fn session(mut self, session: SessionId) -> Self {
        self.session = session;
        self
    }

    /// Override the routing-affinity key, which otherwise defaults to the session id.
    ///
    /// Providers that route across upstreams use this to send related requests to the same one, so
    /// they hit the same warm prompt cache: OpenRouter as `session_id`, OpenAI as
    /// `prompt_cache_key`. Anthropic does not use it — its caching is by explicit breakpoint over an
    /// exact prefix, with no routing decision to influence.
    ///
    /// **The session id is the right default and the wrong one for a specific, common shape.** One
    /// long conversation shares a session and therefore shares a prefix, so keying by session is
    /// exactly right. But a fleet of *short* sessions that share a stable prefix — the same persona
    /// or system prompt run thousands of times — gets a distinct key per session, which scatters
    /// them across upstreams and misses the cache the prefix was built to hit. OpenAI's key needs
    /// sustained traffic to stay warm at all, which no single short session produces.
    ///
    /// For that shape, key by whatever the *prefix* belongs to rather than by the run:
    ///
    /// ```
    /// # use frey_agent::run::Agent;
    /// # fn demo<P: frey_core::provider::ModelProvider, T: frey_agent::run::ToolHost>(
    /// #     provider: P, tools: T,
    /// # ) -> Agent<P, T> {
    /// Agent::new(provider, tools, "some-model")
    ///     .system("…a persona shared by every session this agent runs…")
    ///     .cache_key("persona:ada")
    /// # }
    /// ```
    #[must_use]
    pub fn cache_key(mut self, key: impl Into<smol_str::SmolStr>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    /// Add a provider-specific field to every request this agent makes.
    ///
    /// `Request::extra` has always been merged last by every adapter, so it overrides anything Frey
    /// sets — but nothing exposed it at the agent level, which meant the escape hatch existed and
    /// could not be reached from the only constructor most callers use.
    ///
    /// It is the answer to "this one model needs a flag Frey does not know about", and that case is
    /// not rare on a router fronting hundreds of upstreams. The concrete one:
    /// `meta-llama/llama-3.1-8b-instruct` rejects any request carrying tools unless parallel calls
    /// are disabled, which Frey cannot know per model and will not hardcode.
    ///
    /// ```
    /// # use frey_agent::run::Agent;
    /// # fn demo<P: frey_core::provider::ModelProvider, T: frey_agent::run::ToolHost>(
    /// #     provider: P, tools: T,
    /// # ) -> Agent<P, T> {
    /// Agent::new(provider, tools, "meta-llama/llama-3.1-8b-instruct")
    ///     .extra("parallel_tool_calls", false)
    /// # }
    /// ```
    ///
    /// Nothing validates the key against the provider: this is a passthrough, so a typo reaches the
    /// wire and the provider's own error comes back. That is the intended failure — an allowlist
    /// here would just be another table to keep current.
    #[must_use]
    pub fn extra(
        mut self,
        key: impl Into<smol_str::SmolStr>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.extra.insert(key.into(), value.into());
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
                // The session id unless the caller named something else. See `Agent::cache_key`:
                // the default is right for one long conversation and wrong for many short ones
                // sharing a prefix, and only the caller knows which they are.
                cache_key: Some(
                    self.cache_key.clone().unwrap_or_else(|| self.session.as_str().into()),
                ),
                extra: self.extra.clone(),
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
                    stop: response.stop,
                });
            }

            // 5. Run the tools the model asked for, through the layers — up to the fan-out cap.
            //
            // Refusing the excess individually rather than truncating the list is the whole point.
            // A dropped call produces no result, and a tool call with no result is indistinguishable
            // to the model from one that succeeded and returned nothing, so it proceeds on an
            // invented premise. A refusal it can read makes the next turn a retry with fewer calls.
            let requested = u32::try_from(calls.len()).unwrap_or(u32::MAX);
            if requested > self.max_tool_calls_per_turn {
                warnings.push(Warning::ToolCallsCapped {
                    requested,
                    cap: self.max_tool_calls_per_turn,
                });
            }

            let mut results = Vec::new();
            for (index, call) in calls.into_iter().enumerate() {
                let over_cap =
                    u32::try_from(index).unwrap_or(u32::MAX) >= self.max_tool_calls_per_turn;
                let cx = ToolCx::new(
                    journal.run.clone(),
                    self.session.clone(),
                    frey_core::capability::GrantSet::empty(),
                    frey_core::taint::Provenance::new(format!("tool:{}", call.name)),
                );
                let invocation = Invocation {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.args.clone(),
                    caller: call.caller.clone(),
                };

                // Announced before it runs, not after. A tool call is the part of a run a person
                // most wants to watch happen, and an event emitted only on completion means a slow
                // call shows as nothing at all until it finishes.
                journal.record_event(Event::root(
                    SeqId(turn_index),
                    EventKind::ToolCallStarted {
                        call: call.id.clone(),
                        name: call.name.clone(),
                        args_preview: preview(&call.args),
                    },
                ));
                let started = std::time::Instant::now();

                let definition = definitions.iter().find(|d| d.name == call.name);
                let outcome = if over_cap {
                    ToolOutcome::Denied(
                        ToolError::new(
                            ToolErrorKind::Denied,
                            format!(
                                "this turn asked for {requested} tool calls; only {} may run at once",
                                self.max_tool_calls_per_turn
                            ),
                        )
                        .guide(
                            "This call was not executed. Ask for fewer tools in one turn: make the \
                             calls you most need now, read the results, and continue from there.",
                        ),
                    )
                } else {
                    match definition {
                        None => ToolOutcome::Failed(
                            ToolError::new(
                                ToolErrorKind::NotFound,
                                format!("there is no tool called `{}`", call.name),
                            )
                            // Points only at the list already in the prompt. The earlier wording added
                            // "or search for one", which named an affordance this loop does not
                            // provide — tool search lives in `frey-context` and nothing here consults
                            // it. Guidance that sends a model after a tool that does not exist is this
                            // project's own errors-point-forward principle pointed backwards.
                            .guide("Use one of the tools that were listed."),
                        ),
                        // Policy is consulted before the arguments are checked, and the order is not
                        // cosmetic. Telling a model that its arguments to a forbidden tool are
                        // malformed invites it to correct them and try again, which is a worse answer
                        // than a refusal — and it discloses the tool's schema to a caller that is not
                        // allowed to use it.
                        Some(def) => match PolicyLayer::check(def, &invocation, &cx) {
                            Some(denied) => ToolOutcome::Denied(denied),
                            None => match check_arguments(&def.input_schema, &invocation.args) {
                                Err(invalid) => ToolOutcome::Failed(invalid),
                                Ok(()) => self.tools.call(invocation, &cx).await,
                            },
                        },
                    }
                };

                let (content, is_error, elided) = render_outcome(&outcome);
                let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                journal.record_event(Event::root(
                    SeqId(turn_index),
                    match &outcome {
                        ToolOutcome::Failed(error) | ToolOutcome::Denied(error) => {
                            EventKind::ToolCallFailed {
                                call: call.id.clone(),
                                error: error.clone(),
                            }
                        }
                        _ => EventKind::ToolCallFinished {
                            call: call.id.clone(),
                            millis,
                            bytes_elided: elided,
                        },
                    },
                ));
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

        Err(RunError::TurnLimit { limit: self.max_turns, journal: Box::new(journal) })
    }
}

/// Render a tool outcome for the model, keeping the truncation count so it is never silent.
/// A short rendering of a call's arguments, for a transcript line.
///
/// Truncated hard: arguments can carry a whole file, and a preview that is the payload is not a
/// preview. The full arguments are in the journal's effects for anyone who needs them.
fn preview(args: &serde_json::Value) -> String {
    const MAX: usize = 120;
    let rendered = args.to_string();
    if rendered.len() <= MAX {
        return rendered;
    }
    // On a character boundary, because arguments are arbitrary text and slicing mid-codepoint
    // panics — in the transcript path, which is the last place a run should die.
    let cut = rendered.char_indices().map(|(i, _)| i).take_while(|i| *i <= MAX).last().unwrap_or(0);
    format!("{}…", &rendered[..cut])
}

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
            // **What the encoder will actually send, not what the item holds.** These two used to
            // disagree, and the disagreement killed runs. `format!("{other:?}")` caught
            // `Item::Reasoning` and charged the budget for the Debug representation of a struct the
            // Chat Completions dialect drops on the floor — so a model that reasoned at length was
            // billed here for text that never reached the wire. One agent was refused a turn over
            // "history (971444 tokens)": a prompt of about four megabytes, none of which would have
            // been sent.
            //
            // A Debug representation is never a proxy for wire size. Anything a dialect does not
            // encode weighs nothing, and an estimate that guesses otherwise is not an estimate of
            // the prompt.
            .map(|i| match i {
                Item::Text(t) => t.text.clone(),
                Item::ToolCall(c) => format!("{}{}", c.name, c.args),
                Item::ToolResult(r) => r.content.clone(),
                // Reasoning, media and provider blocks: carried in the item model, absent from the
                // request. If a dialect learns to send one, its cost belongs here with it.
                _ => String::new(),
            })
            // A dropped item leaves nothing behind, not even the separator it would have been
            // joined with. One newline per discarded block is small, and it is still not the prompt.
            .filter(|part| !part.is_empty())
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

    fn numbered_call(name: &str, n: usize) -> Item {
        Item::ToolCall(ToolCallItem {
            id: CallId::new(format!("c{n}")),
            name: ToolName::new(name),
            args: serde_json::json!({}),
            caller: Caller::Direct,
        })
    }

    /// Every tool call appears in the transcript, started and finished.
    ///
    /// It did not, for the whole life of the crate. `ToolCallStarted`, `ToolCallFinished` and
    /// `ToolCallFailed` were defined in `frey-core`, translated into AG-UI frames by
    /// `frey-harness`, and unit-tested in both — and the loop that actually runs tools emitted none
    /// of them. Anything watching a run saw a stream with no tool activity in it at all, which is
    /// most of what a run *is*. Nothing caught it because every test asserted on the events it
    /// constructed itself; the first caller to read `journal.events` back off a real run got zero.
    #[test]
    fn a_tool_call_is_visible_in_the_transcript() {
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(vec![tool_call("fs_read")]),
            Scripted::text("done"),
        ]);
        let agent = Agent::new(model, tools(&["fs_read"]), "test-model");
        let output = pollster::block_on(agent.run("read it")).unwrap();

        let started: Vec<_> = output
            .journal
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolCallStarted { name, .. } => Some(name.as_ref().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(started, vec!["fs_read"], "the call has to be announced");
        assert!(
            output
                .journal
                .events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolCallFinished { .. })),
            "and its completion recorded"
        );
    }

    /// A refused call is reported as failed, not as finished with an error buried in its output.
    #[test]
    fn a_refused_tool_call_is_reported_as_failed() {
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(vec![tool_call("nonexistent")]),
            Scripted::text("oh"),
        ]);
        let agent = Agent::new(model, tools(&["fs_read"]), "test-model");
        let output = pollster::block_on(agent.run("go")).unwrap();

        assert!(
            output
                .journal
                .events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolCallFailed { .. })),
            "a call that did not happen must not read as one that did"
        );
    }

    /// A single response asking for a great many tools does not get to run them all.
    ///
    /// From live traffic rather than imagination: `meta-llama/llama-3.1-8b-instruct` emitted
    /// roughly 145 tool calls in one response, and before this cap existed the loop executed every
    /// one of them — 267 journal effects and ten times the cost of the successful run.
    #[test]
    fn a_runaway_response_is_capped_rather_than_executed() {
        let flood: Vec<Item> = (0..40).map(|n| numbered_call("fs_read", n)).collect();
        let model = ScriptedModel::new(vec![
            Scripted::tool_calls(flood),
            Scripted::text("I will slow down."),
        ]);
        let agent = Agent::new(model, tools(&["fs_read"]), "test-model").max_tool_calls_per_turn(5);
        let out = pollster::block_on(agent.run("read everything")).unwrap();

        let executed = out
            .journal
            .entries
            .iter()
            .filter(|e| matches!(&e.effect, Effect::ToolResult { is_error: false, .. }))
            .count();
        assert_eq!(executed, 5, "only the permitted calls run");

        let refused = out
            .journal
            .entries
            .iter()
            .filter(|e| matches!(&e.effect, Effect::ToolResult { is_error: true, .. }))
            .count();
        assert_eq!(refused, 35, "the rest are refused, not silently dropped");

        assert!(
            out.warnings.contains(&Warning::ToolCallsCapped { requested: 40, cap: 5 }),
            "the operator is told, in numbers: {:?}",
            out.warnings
        );
    }

    /// The refusal has to reach the model, or it will assume the call succeeded and returned
    /// nothing — which is a worse failure than the runaway, because it is silent.
    #[test]
    fn a_capped_call_tells_the_model_what_to_do_instead() {
        let flood: Vec<Item> = (0..10).map(|n| numbered_call("fs_read", n)).collect();
        let model =
            ScriptedModel::new(vec![Scripted::tool_calls(flood), Scripted::text("understood")]);
        let agent =
            Agent::new(model.clone(), tools(&["fs_read"]), "test-model").max_tool_calls_per_turn(2);
        pollster::block_on(agent.run("read everything")).unwrap();

        // What the model was shown on its second turn contains the guidance, not just a failure.
        let second = &model.saw()[1];
        let shown: String = second
            .turns
            .iter()
            .flat_map(|t| t.items.iter())
            .filter_map(|i| match i {
                Item::ToolResult(r) => Some(r.content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(shown.contains("only 2 may run at once"), "the limit is named: {shown}");
        assert!(shown.contains("Ask for fewer tools"), "and what to do about it: {shown}");
    }

    /// Routing affinity defaults to the session id, which is right for one long conversation:
    /// everything in it shares a prefix, so everything in it should reach the same warm cache.
    #[test]
    fn routing_affinity_defaults_to_the_session() {
        let model = ScriptedModel::replying("done");
        let agent = Agent::new(model.clone(), tools(&[]), "test-model")
            .session(SessionId::new("session-42"));
        pollster::block_on(agent.run("go")).unwrap();

        assert_eq!(model.last().cache_key.as_deref(), Some("session-42"));
    }

    /// And is overridable, because the default is wrong for the opposite shape: many short sessions
    /// sharing one stable prefix get a distinct key each, which scatters them across upstreams and
    /// misses the cache the prefix exists to hit. Only the caller knows which shape they are.
    #[test]
    fn routing_affinity_can_be_keyed_to_the_prefix_instead() {
        let model = ScriptedModel::replying("done");
        let agent = Agent::new(model.clone(), tools(&[]), "test-model")
            .session(SessionId::new("session-42"))
            .cache_key("persona:ada");
        pollster::block_on(agent.run("go")).unwrap();

        assert_eq!(model.last().cache_key.as_deref(), Some("persona:ada"));
    }

    /// Two runs that share a persona and nothing else must present the same key, or the override
    /// has not bought anything. This is the property the whole setter exists for.
    #[test]
    fn two_sessions_sharing_a_persona_share_a_key() {
        let keys: Vec<_> = ["session-1", "session-2"]
            .iter()
            .map(|session| {
                let model = ScriptedModel::replying("done");
                let agent = Agent::new(model.clone(), tools(&[]), "test-model")
                    .session(SessionId::new(*session))
                    .cache_key("persona:ada");
                pollster::block_on(agent.run("go")).unwrap();
                model.last().cache_key.clone()
            })
            .collect();

        assert_eq!(keys[0], keys[1], "a shared prefix must present a shared key");
    }

    /// The escape hatch has to be reachable from the constructor callers actually use. `extra` was
    /// merged by every adapter and exposed by nothing, which is an escape hatch behind a locked
    /// door.
    #[test]
    fn provider_specific_fields_reach_the_request() {
        let model = ScriptedModel::replying("done");
        let agent = Agent::new(model.clone(), tools(&[]), "meta-llama/llama-3.1-8b-instruct")
            .extra("parallel_tool_calls", false);
        pollster::block_on(agent.run("go")).unwrap();

        assert_eq!(model.last().extra.get("parallel_tool_calls"), Some(&serde_json::json!(false)));
    }

    /// An answer cut off at the output cap is returned rather than discarded — the content is real
    /// and throwing it away would be worse — but it is not a finished answer, and the caller can
    /// tell from a value instead of from prose in `warnings`.
    #[test]
    fn a_truncated_answer_does_not_claim_to_be_complete() {
        let model = ScriptedModel::new(vec![Scripted::truncated("The three steps are: first, ")]);
        let out = pollster::block_on(
            Agent::new(model, tools(&[]), "test-model").run("explain the process"),
        )
        .unwrap();

        assert!(!out.is_complete(), "the model hit its cap mid-sentence");
        assert_eq!(out.stop, StopReason::MaxTokens);
        assert!(!out.text().is_empty(), "and the partial answer is still returned");
        assert!(
            out.warnings.iter().any(|w| matches!(w, Warning::Degraded { .. })),
            "the warning stays too; the field is for telling, the warning is for reading"
        );
    }

    /// The ordinary case has to stay ordinary, or the flag is just noise.
    #[test]
    fn a_finished_answer_says_it_is_complete() {
        let model = ScriptedModel::replying("done");
        let out =
            pollster::block_on(Agent::new(model, tools(&[]), "test-model").run("go")).unwrap();

        assert!(out.is_complete());
        assert_eq!(out.stop, StopReason::EndTurn);
    }

    /// The default has to be high enough that ordinary parallel fan-out is untouched. A model that
    /// reads six files at once is behaving well, and a cap that punishes it would push agents
    /// toward the serial pattern that costs extra round trips.
    #[test]
    fn ordinary_parallel_fan_out_is_not_capped() {
        let calls: Vec<Item> = (0..8).map(|n| numbered_call("fs_read", n)).collect();
        let model = ScriptedModel::new(vec![Scripted::tool_calls(calls), Scripted::text("done")]);
        let out = pollster::block_on(
            Agent::new(model, tools(&["fs_read"]), "test-model").run("read the tree"),
        )
        .unwrap();

        assert!(
            !out.warnings.iter().any(|w| matches!(w, Warning::ToolCallsCapped { .. })),
            "eight parallel reads is normal behaviour, not a runaway"
        );
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
        assert!(
            content.contains("Use one of the tools that were listed"),
            "a bare failure just loops: {content}"
        );
        // The guidance must not name an affordance this loop does not provide. Tool search lives in
        // `frey-context` and nothing in the loop consults it, so telling a model to search for a
        // tool sends it hunting for something that is not there — and a model that takes the advice
        // burns a turn proving it.
        assert!(
            !content.contains("search"),
            "guidance may only point at what the loop actually offers: {content}"
        );
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
        let RunError::TurnLimit { limit, journal } = &err else { panic!("{err}") };
        assert_eq!(*limit, 3);
        assert!(format!("{err}").contains("repeating the same failing tool call"));
        // The advice is to read the transcript, so the transcript has to come back with it.
        assert!(
            journal.events.iter().any(|e| matches!(e.kind, EventKind::ToolCallStarted { .. })),
            "a looping run is exactly when its record matters"
        );
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

#[cfg(test)]
mod estimate_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use frey_core::item::{ReasoningItem, ReasoningVisibility, TextItem, ToolResultItem};
    use frey_core::taint::Provenance;

    /// **The estimate must measure what the encoder sends.** These two disagreed, and the
    /// disagreement killed runs rather than degrading them: `format!("{other:?}")` caught
    /// `Item::Reasoning` and charged the budget for the Debug representation of a struct the Chat
    /// Completions dialect drops. An agent was refused a turn over "history (971444 tokens)" — about
    /// four megabytes, none of which would have been sent, on a world whose largest page was 3.7 kB.
    ///
    /// The budgeter then behaved correctly on a false premise, which is the worst way to be wrong:
    /// it protected the last exchanges, found nothing left to evict, and returned `DoesNotFit` with
    /// numbers that looked authoritative.
    #[test]
    fn reasoning_costs_nothing_because_nothing_sends_it() {
        let long = "thinking out loud. ".repeat(50_000);

        let quiet = vec![Turn {
            role: Role::Assistant,
            items: vec![Item::Text(TextItem { text: "done".to_string(), provenance: None })],
        }];
        let loud = vec![Turn {
            role: Role::Assistant,
            items: vec![
                Item::Reasoning(ReasoningItem {
                    summary: Some(long.clone()),
                    visibility: ReasoningVisibility::Plain,
                    carry: None,
                }),
                Item::Text(TextItem { text: "done".to_string(), provenance: None }),
            ],
        }];

        let cheap = build_segments(&[], &quiet);
        let dear = build_segments(&[], &loud);
        assert_eq!(
            cheap[0].est_tokens, dear[0].est_tokens,
            "a megabyte of reasoning the encoder discards must not be billed to the prompt"
        );
    }

    /// And what *is* sent still counts, so this did not become a free pass.
    #[test]
    fn a_tool_result_still_weighs_what_it_weighs() {
        let turns = vec![Turn {
            role: Role::User,
            items: vec![Item::ToolResult(ToolResultItem {
                id: frey_core::ids::CallId::new("c1"),
                content: "x".repeat(4_000),
                is_error: false,
                bytes_elided: 0,
                provenance: Provenance::new("test"),
            })],
        }];
        assert_eq!(build_segments(&[], &turns)[0].est_tokens, 1_000);
    }
}
