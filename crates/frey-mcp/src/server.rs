//! Being an MCP server, at the stateless `2026-07-28` revision.
//!
//! Frey shipped v0.1.1 able to *consume* MCP and unable to *be* it, which dogfooding found within
//! an hour: two of the three demo projects wanted a Frey-built server and there was nothing to
//! build one with. For a framework whose first claim is "MCP-native", in a revision whose whole
//! point is what a stateless server looks like, that was a gap rather than a missing convenience.
//!
//! # What this is
//!
//! [`Server`] turns any [`Toolset`] into an MCP endpoint. That is the thesis the rest of the
//! framework is built on — tools, skills, and code-mode are presentations of one catalog — applied
//! one level out: the *same* toolset an agent calls in-process is the one a remote client calls
//! over the wire, with no second registration and no divergence between them.
//!
//! # What it deliberately is not
//!
//! There is no transport here. [`Server::handle`] takes a JSON value and returns one, so the whole
//! protocol is testable without a socket, and stdio, HTTP, or a unix pipe is a dozen lines the
//! caller writes. That is the same split as [`Dialect`](frey_providers) versus `HttpProvider`, for
//! the same reason: the interesting behaviour is the mapping, and a network in the test loop only
//! obscures it.
//!
//! # Statelessness is the load-bearing property
//!
//! This revision deleted the handshake and the session id. A request carries everything needed to
//! serve it, which means any instance can answer any request and horizontal scaling is free. The
//! test [`tests::two_servers_answer_identically`] holds the line: nothing may accumulate in
//! `Server` between calls, because the second replica does not have it.

use frey_core::error::{ToolError, ToolErrorKind, ToolOutcome};
use frey_core::ids::{CallId, RunId, SessionId, ToolName};
use frey_core::item::Caller;
use frey_core::taint::Provenance;
use frey_core::tool::{Invocation, Resume, StepCx, ToolCx, Toolset};
use frey_core::tool_def::ToolDefinition;
use frey_core::validate::check_arguments;
use serde_json::{Value, json};
use smol_str::SmolStr;

use crate::protocol::{CacheScope, McpTool, PROTOCOL_VERSION, meta_keys};

/// JSON-RPC error codes this server produces.
pub mod codes {
    /// The payload was not valid JSON-RPC.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method. A client uses this to discover what revision it is talking to, so it must
    /// be returned rather than a generic failure.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The parameters were wrong.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Something went wrong inside the server.
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// How this server describes itself to `server/discover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// Server name.
    pub name: SmolStr,
    /// Server version.
    pub version: SmolStr,
    /// Prose shown to a model alongside the tools. Keep it short: it is prompt, and it is paid for
    /// on every request that includes it.
    pub instructions: Option<String>,
}

/// An MCP server over a [`Toolset`].
#[derive(Debug, Clone)]
pub struct Server<T> {
    toolset: T,
    info: ServerInfo,
    ttl_ms: Option<u64>,
    cache_scope: CacheScope,
}

impl<T: Toolset> Server<T> {
    /// A server exposing `toolset`.
    pub fn new(name: impl Into<SmolStr>, version: impl Into<SmolStr>, toolset: T) -> Self {
        Self {
            toolset,
            info: ServerInfo { name: name.into(), version: version.into(), instructions: None },
            // Five minutes by default. A listing that never expires pins a stale catalog on every
            // client; one that expires instantly throws away the cache this revision added the
            // field to protect.
            ttl_ms: Some(300_000),
            // Private unless the operator says otherwise. A shared cache key across principals is
            // how one tenant's tool list reaches another, and defaulting to the fast answer would
            // make that the common case.
            cache_scope: CacheScope::Private,
        }
    }

    /// Prose the client should show the model when these tools are visible.
    #[must_use]
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.info.instructions = Some(text.into());
        self
    }

    /// How long a client may treat a listing as fresh. `None` means "do not cache".
    #[must_use]
    pub fn ttl_ms(mut self, ttl: Option<u64>) -> Self {
        self.ttl_ms = ttl;
        self
    }

    /// Who may cache a listing. Only say [`CacheScope::Public`] when the catalog genuinely does not
    /// vary by caller — it authorises shared intermediaries to serve one principal's listing to
    /// another.
    #[must_use]
    pub fn cache_scope(mut self, scope: CacheScope) -> Self {
        self.cache_scope = scope;
        self
    }

    /// Handle one JSON-RPC message.
    ///
    /// Returns `None` for a notification — a message with no `id` — because JSON-RPC forbids
    /// answering one, and a server that replies anyway desynchronises a client that is counting
    /// responses.
    pub async fn handle(&self, raw: &Value) -> Option<Value> {
        let id = raw.get("id").cloned();
        let method = raw.get("method").and_then(Value::as_str);

        // A notification. Nothing to say, whatever it asked for.
        let id = id?;

        let Some(method) = method else {
            return Some(error(&id, codes::INVALID_REQUEST, "request has no `method`"));
        };

        let params = raw.get("params").cloned().unwrap_or(Value::Null);
        let trace = raw
            .get("_meta")
            .and_then(|m| m.get(meta_keys::TRACEPARENT))
            .and_then(Value::as_str)
            .map(ToString::to_string);

        let result = match method {
            "server/discover" => Ok(self.discover()),
            // `initialize` belongs to the revision this one replaced. Answering it is a deliberate
            // compatibility shim: a pre-stateless client sends it first and gives up on an error,
            // and refusing would make Frey's servers unusable from every client shipped before
            // 2026-07-28 for no protocol benefit.
            "initialize" => Ok(self.initialize()),
            "tools/list" => self.list_tools().await,
            "tools/call" => self.call_tool(&params).await,
            other => Err(RpcError {
                code: codes::METHOD_NOT_FOUND,
                message: format!("this server does not implement `{other}`"),
            }),
        };

        Some(match result {
            Ok(mut value) => {
                // Echo the trace parent so a client correlating spans across the call boundary can
                // find this one. It is diagnostic only and never affects the answer.
                if let Some(trace) = trace {
                    value["_meta"] = json!({ meta_keys::TRACEPARENT: trace });
                }
                json!({"jsonrpc": "2.0", "id": id, "result": value})
            }
            Err(e) => error(&id, e.code, &e.message),
        })
    }

    fn discover(&self) -> Value {
        let mut value = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {"name": self.info.name, "version": self.info.version},
            // No session id, by design rather than by omission. Advertising statelessness is what
            // lets a client skip the handshake entirely.
            "stateless": true,
            "capabilities": {"tools": {}},
        });
        if let Some(instructions) = &self.info.instructions {
            value["instructions"] = Value::String(instructions.clone());
        }
        value
    }

    fn initialize(&self) -> Value {
        let mut value = self.discover();
        value["protocolVersion"] = Value::String(PROTOCOL_VERSION.to_string());
        value
    }

    /// One tool's definition, for validating a call against it.
    ///
    /// Asking the toolset again rather than caching is deliberate: visibility is a function of the
    /// current step, and a server that validated against a stale copy of a schema would reject
    /// calls that are now correct.
    async fn definition_of(&self, name: &str) -> Option<ToolDefinition> {
        let cx = StepCx {
            run: RunId::new("mcp"),
            session: SessionId::new("stateless"),
            task: String::new(),
            tokens_available: u32::MAX,
        };
        self.toolset.definitions(&cx).await.ok()?.into_iter().find(|d| d.name.as_str() == name)
    }

    async fn list_tools(&self) -> Result<Value, RpcError> {
        let cx = StepCx {
            run: RunId::new("mcp"),
            session: SessionId::new("stateless"),
            task: String::new(),
            // A stateless server has no idea what the caller's budget is, and inventing a number
            // would silently hide tools. Presentation is the client's job; the server's job is to
            // say what exists.
            tokens_available: u32::MAX,
        };

        let definitions = self
            .toolset
            .definitions(&cx)
            .await
            .map_err(|e| RpcError { code: codes::INTERNAL_ERROR, message: e.to_string() })?;

        let mut tools: Vec<McpTool> = definitions
            .into_iter()
            .map(|d| McpTool {
                name: d.name.to_string(),
                description: d.description.to_string(),
                input_schema: d.input_schema.as_value().clone(),
                output_schema: d.output_schema.map(|s| s.as_value().clone()),
            })
            .collect();

        // Sorted before it goes out. The specification asks servers to be deterministic so that a
        // client's prompt cache is not invalidated by a reordering, and Frey's own client re-sorts
        // defensively because servers ignore that. Being on the other side is no excuse for being
        // the server that makes everyone else pay.
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        let mut value = json!({"tools": tools, "cacheScope": self.cache_scope});
        if let Some(ttl) = self.ttl_ms {
            value["ttlMs"] = json!(ttl);
        }
        Ok(value)
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, RpcError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(RpcError {
                code: codes::INVALID_PARAMS,
                message: "`tools/call` needs a `name`".into(),
            });
        };
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        // Validate against the tool's own declared schema before dispatching, exactly as the agent
        // loop does. Skipping this here made the *same toolset* behave differently depending on
        // whether an agent or a remote client called it — a demo project found it by sending
        // `"value": "true"` to a boolean parameter and watching it be read as `false`.
        //
        // A remote caller is less trustworthy than the local model, not more, so if either surface
        // were going to skip validation it should not have been this one.
        if let Some(definition) = self.definition_of(name).await
            && let Err(invalid) = check_arguments(&definition.input_schema, &arguments)
        {
            return Ok(tool_error(&invalid));
        }

        let mut cx = ToolCx::new(
            RunId::new("mcp"),
            SessionId::new("stateless"),
            // Empty rather than permissive. A server hands out no capability it was not built with;
            // whatever the toolset needs, it holds itself, and a remote caller cannot widen it.
            frey_core::capability::GrantSet::empty(),
            Provenance::new(format!("mcp:{}/{name}", self.info.name)),
        );

        // The other half of the multi round-trip pattern. A tool that needed input returned what it
        // wanted plus a sealed state; the client re-sends the *same call* with answers attached,
        // and because nothing was remembered here, those answers are the only way the tool can
        // continue. A server that emits `input_required` and cannot read the retry has implemented
        // half a handshake.
        if let Some(answers) = params.get("inputResponses").and_then(Value::as_array) {
            cx = cx.resuming(Resume {
                state: params.get("requestState").cloned().unwrap_or(Value::Null),
                answers: answers.clone(),
            });
        }
        let invocation = Invocation {
            id: CallId::new("mcp-call"),
            name: ToolName::new(name),
            args: arguments,
            caller: Caller::Direct,
        };

        Ok(match self.toolset.call(invocation, &cx).await {
            ToolOutcome::Ok(value) => {
                let content = value.peek();
                let mut result = json!({
                    "content": [{"type": "text", "text": content.text}],
                    "isError": false,
                });
                if let Some(structured) = &content.structured {
                    result["structuredContent"] = structured.clone();
                }
                result
            }
            // A tool that fails reports it *in the result*, not as a JSON-RPC error. The
            // distinction is the whole reason tool errors are useful: a protocol error is the
            // client's problem and never reaches the model, while this does — carrying the guidance
            // that turns an identical retry into a corrected one.
            ToolOutcome::Failed(e) | ToolOutcome::Denied(e) => tool_error(&e),
            // The multi round-trip pattern: rather than calling back to the client, the server says
            // what it needs and the client retries the whole request with answers attached. This is
            // what makes statelessness possible at all.
            ToolOutcome::NeedsInput(needs) => json!({
                "resultType": "input_required",
                "inputRequests": needs.requests.iter().map(input_request).collect::<Vec<_>>(),
                // The resume token *is* the request state. Sealed by this server, opaque to the
                // client, and the reason a stateless server can suspend a call at all: the client
                // hands it straight back on the retry and nothing had to be remembered here.
                "requestState": {"token": needs.token},
            }),
            // `ToolOutcome` is `#[non_exhaustive]`, so a future variant lands here. Reporting it as
            // a tool error rather than guessing keeps a newly-added outcome from being silently
            // rendered as success by an older server build.
            other => json!({
                "content": [{
                    "type": "text",
                    "text": format!("this server does not know how to report a `{other:?}` outcome"),
                }],
                "isError": true,
            }),
        })
    }
}

/// A JSON-RPC level failure.
struct RpcError {
    code: i64,
    message: String,
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Render a tool failure as a *result* the model can read.
///
/// Not a JSON-RPC error, and the distinction is the whole reason typed tool errors are useful: a
/// protocol error is the client's problem and stops there, while this reaches the model carrying
/// the guidance that makes the next attempt different from the last.
///
/// Shared by the validation path and the dispatch path so the two cannot produce different shapes
/// for what a client sees as the same kind of failure.
fn tool_error(error: &ToolError) -> Value {
    let model = error.model();
    let mut text = model.summary.clone();
    if let Some(guidance) = &model.guidance {
        text.push(' ');
        text.push_str(guidance);
    }
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": true,
        "_meta": {"io.modelcontextprotocol/errorKind": kind_name(error.kind())},
    })
}

fn kind_name(kind: ToolErrorKind) -> &'static str {
    match kind {
        ToolErrorKind::NotFound => "not_found",
        ToolErrorKind::InvalidArgs => "invalid_args",
        ToolErrorKind::Denied => "denied",
        ToolErrorKind::Conflict => "conflict",
        ToolErrorKind::Timeout => "timeout",
        ToolErrorKind::Transient => "transient",
        ToolErrorKind::Fatal => "fatal",
        ToolErrorKind::Cancelled => "cancelled",
        _ => "other",
    }
}

/// Project one of Frey's input requests onto the wire.
///
/// This is the same projection A2A and AG-UI get, which is the payoff from ADR-0010: MCP's
/// multi round-trip result, A2A's `INPUT_REQUIRED` task, and an AG-UI interrupt are the same event,
/// so there is one type and three serialisers rather than three half-compatible flows.
///
/// `Approval` carries the literal action rather than a summary, here as everywhere. A person
/// approving a command needs to see the command; a natural-language paraphrase is precisely where
/// an injected instruction survives review.
fn input_request(request: &frey_core::error::InputRequest) -> Value {
    use frey_core::error::InputRequest as R;
    match request {
        R::Approval { literal, risk } => {
            json!({"kind": "approval", "literal": literal, "risk": format!("{risk:?}").to_lowercase()})
        }
        R::Choice { prompt, options } => {
            json!({"kind": "choice", "prompt": prompt, "options": options})
        }
        R::Form { schema } => json!({"kind": "form", "schema": schema}),
        R::Auth { resource } => json!({"kind": "auth", "resource": resource}),
        // A frontend tool is something only the client can run, so a server asking for one is
        // asking the client to act rather than to answer. Naming it as its own kind keeps a client
        // from rendering it as a text prompt to a human who cannot help.
        other => json!({"kind": "other", "detail": format!("{other:?}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::error::ToolError;
    use frey_core::taint::Tainted;
    use frey_core::tool::{ToolContent, ToolValue, ToolsetError};
    use frey_core::tool_def::{JsonSchema, ToolDefinition};

    /// Two tools, registered in an order that is not their sorted order, so the sorting test is
    /// measuring something.
    struct Notes;

    impl Toolset for Notes {
        fn name(&self) -> SmolStr {
            "notes".into()
        }

        async fn definitions(&self, _cx: &StepCx) -> Result<Vec<ToolDefinition>, ToolsetError> {
            Ok(vec![
                ToolDefinition::new(
                    "note_write",
                    "Write a note to the store.",
                    JsonSchema::new(json!({
                        "type": "object",
                        "properties": {"text": {"type": "string", "description": "The note body."}},
                        "required": ["text"]
                    }))
                    .unwrap(),
                ),
                ToolDefinition::new(
                    "note_count",
                    "Count the notes in the store.",
                    JsonSchema::empty_object(),
                ),
            ])
        }

        async fn call(&self, invocation: Invocation, cx: &ToolCx) -> ToolOutcome<ToolValue> {
            match invocation.name.as_str() {
                "note_count" => ToolOutcome::Ok(Tainted::with_provenance(
                    ToolContent::text("3"),
                    cx.provenance.clone(),
                )),
                "note_write" => ToolOutcome::Failed(
                    ToolError::new(ToolErrorKind::Denied, "the store is read-only today")
                        .guide("Try note_count instead; writing is disabled."),
                ),
                other => ToolOutcome::Failed(ToolError::new(
                    ToolErrorKind::NotFound,
                    format!("no tool `{other}`"),
                )),
            }
        }
    }

    fn server() -> Server<Notes> {
        Server::new("notes-server", "1.0.0", Notes)
    }

    /// A tool that will not act until a human approves the literal action.
    struct Gated;

    impl Toolset for Gated {
        fn name(&self) -> SmolStr {
            "gated".into()
        }

        async fn definitions(&self, _cx: &StepCx) -> Result<Vec<ToolDefinition>, ToolsetError> {
            Ok(vec![ToolDefinition::new(
                "deploy",
                "Deploy a version to production.",
                JsonSchema::new(json!({
                    "type": "object",
                    "properties": {"version": {"type": "string", "description": "Version to deploy."}},
                    "required": ["version"]
                }))
                .unwrap(),
            )])
        }

        async fn call(&self, invocation: Invocation, cx: &ToolCx) -> ToolOutcome<ToolValue> {
            let version = invocation.args.get("version").and_then(Value::as_str).unwrap_or("?");

            match &cx.resume {
                // First attempt: say what is needed and seal the state. Nothing is remembered here.
                None => ToolOutcome::NeedsInput(frey_core::error::NeedsInput {
                    token: "deploy-pending".into(),
                    requests: vec![frey_core::error::InputRequest::Approval {
                        literal: format!("deploy {version} to production"),
                        risk: frey_core::error::Risk::High,
                    }],
                }),
                Some(resume) => {
                    let approved = resume.answers.first().and_then(Value::as_bool).unwrap_or(false);
                    if approved {
                        ToolOutcome::Ok(Tainted::with_provenance(
                            ToolContent::text(format!("deployed {version}")),
                            cx.provenance.clone(),
                        ))
                    } else {
                        ToolOutcome::Denied(ToolError::new(
                            ToolErrorKind::Denied,
                            "the operator declined the deployment",
                        ))
                    }
                }
            }
        }
    }

    fn gated_call(params: Value) -> Value {
        let server = Server::new("gated", "1.0.0", Gated);
        pollster::block_on(server.handle(&request(1, "tools/call", params))).unwrap()
    }

    /// The multi round-trip pattern, both halves. A server that emits `input_required` and cannot
    /// read the retry has implemented half a handshake — which is what Frey shipped until this
    /// test existed.
    #[test]
    fn a_tool_that_needs_input_can_be_resumed_by_a_retry() {
        let first = gated_call(json!({"name": "deploy", "arguments": {"version": "4.2"}}));
        assert_eq!(first["result"]["resultType"], "input_required");

        // The approval shows the literal action, never a paraphrase: a summary is exactly where an
        // injected instruction survives review by the person clicking yes.
        let asked = &first["result"]["inputRequests"][0];
        assert_eq!(asked["kind"], "approval");
        assert_eq!(asked["literal"], "deploy 4.2 to production");
        assert_eq!(asked["risk"], "high");

        let state = first["result"]["requestState"].clone();
        let second = gated_call(json!({
            "name": "deploy",
            "arguments": {"version": "4.2"},
            "requestState": state,
            "inputResponses": [true],
        }));
        assert_eq!(second["result"]["isError"], false);
        assert_eq!(second["result"]["content"][0]["text"], "deployed 4.2");
    }

    /// A refusal on the retry is a refusal, not a second prompt.
    #[test]
    fn a_declined_approval_denies_the_call() {
        let denied = gated_call(json!({
            "name": "deploy",
            "arguments": {"version": "4.2"},
            "requestState": {"token": "deploy-pending"},
            "inputResponses": [false],
        }));
        assert_eq!(denied["result"]["isError"], true);
        assert!(denied["result"]["content"][0]["text"].as_str().unwrap().contains("declined"));
    }

    /// An absent answer is not a yes. A retry that carries no responses must be treated as a first
    /// attempt and ask again, rather than proceeding on silence.
    #[test]
    fn a_retry_without_answers_asks_again_rather_than_assuming_approval() {
        let again = gated_call(json!({
            "name": "deploy",
            "arguments": {"version": "4.2"},
            "requestState": {"token": "deploy-pending"},
        }));
        assert_eq!(again["result"]["resultType"], "input_required");
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn call(raw: &Value) -> Value {
        pollster::block_on(server().handle(raw)).expect("a request with an id gets an answer")
    }

    #[test]
    fn discover_advertises_the_stateless_revision() {
        let reply = call(&request(1, "server/discover", Value::Null));
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["stateless"], true);
        assert_eq!(result["serverInfo"]["name"], "notes-server");
        assert!(
            result.get("sessionId").is_none(),
            "a stateless server issues no session id: {result}"
        );
    }

    /// The specification asks servers to be deterministic so a client's prompt cache survives.
    /// Frey's own client re-sorts defensively because servers ignore that; being on the other side
    /// is no excuse for being the server everyone else has to defend against.
    #[test]
    fn tools_are_listed_in_a_stable_order() {
        let reply = call(&request(1, "tools/list", Value::Null));
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["note_count", "note_write"], "sorted, not registration order");
    }

    #[test]
    fn a_listing_carries_the_freshness_hints_this_revision_added() {
        let reply = call(&request(1, "tools/list", Value::Null));
        assert_eq!(reply["result"]["ttlMs"], 300_000);
        assert_eq!(reply["result"]["cacheScope"], "private");
    }

    /// `Public` authorises a shared intermediary to serve one principal's listing to another, so it
    /// has to be asked for rather than assumed.
    #[test]
    fn cache_scope_is_private_until_the_operator_says_otherwise() {
        let public = Server::new("s", "1", Notes).cache_scope(CacheScope::Public);
        let reply =
            pollster::block_on(public.handle(&request(1, "tools/list", Value::Null))).unwrap();
        assert_eq!(reply["result"]["cacheScope"], "public");
        assert_eq!(call(&request(1, "tools/list", Value::Null))["result"]["cacheScope"], "private");
    }

    #[test]
    fn a_successful_call_returns_content() {
        let reply = call(&request(1, "tools/call", json!({"name": "note_count", "arguments": {}})));
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(reply["result"]["content"][0]["text"], "3");
    }

    /// A tool failure is a *result*, not a JSON-RPC error. The difference decides whether the model
    /// ever sees it: a protocol error is the client's problem and stops there, while this reaches
    /// the model carrying the guidance that makes the next attempt different from the last.
    #[test]
    fn a_tool_failure_is_a_result_the_model_can_read_not_a_protocol_error() {
        let reply = call(&request(
            1,
            "tools/call",
            json!({"name": "note_write", "arguments": {"text": "x"}}),
        ));
        assert!(reply.get("error").is_none(), "not a JSON-RPC error: {reply}");
        assert_eq!(reply["result"]["isError"], true);

        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("read-only"), "the summary: {text}");
        assert!(text.contains("Try note_count instead"), "and the guidance: {text}");
    }

    /// The agent loop validates arguments against the declared schema before dispatch. So must
    /// this: the same toolset behaving differently depending on whether an agent or a remote client
    /// called it is exactly the divergence "one catalog, many presentations" exists to prevent —
    /// and a remote caller is the *less* trustworthy of the two.
    ///
    /// Found by a demo project sending `"value": "true"` to a boolean parameter and watching the
    /// tool's own `as_bool` fallback read it as `false`.
    #[test]
    fn arguments_are_validated_before_the_tool_runs() {
        let reply = call(&request(
            1,
            "tools/call",
            json!({"name": "note_write", "arguments": {"text": 42}}),
        ));

        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["_meta"]["io.modelcontextprotocol/errorKind"], "invalid_args");
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("`text` must be a string"), "{text}");
    }

    /// A missing required argument is refused here rather than reaching tool code, which would
    /// otherwise have to hand-roll the same check in every tool.
    #[test]
    fn a_missing_required_argument_never_reaches_the_tool() {
        let reply = call(&request(1, "tools/call", json!({"name": "note_write", "arguments": {}})));
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("`text` is missing"), "{text}");
        assert!(
            !text.contains("read-only"),
            "the tool did not run, so its own message is absent: {text}"
        );
    }

    /// A client discovers what revision it is talking to by getting this code back, so it must be
    /// method-not-found specifically rather than a generic failure.
    #[test]
    fn an_unknown_method_is_method_not_found_so_a_client_can_negotiate() {
        let reply = call(&request(1, "resources/list", Value::Null));
        assert_eq!(reply["error"]["code"], codes::METHOD_NOT_FOUND);
    }

    /// JSON-RPC forbids answering a notification, and a server that replies anyway desynchronises a
    /// client counting responses.
    #[test]
    fn a_notification_gets_no_answer() {
        let notification = json!({"jsonrpc": "2.0", "method": "notifications/progress"});
        assert!(pollster::block_on(server().handle(&notification)).is_none());
    }

    /// The property the whole revision rests on. If anything accumulated in `Server` between
    /// requests, a second replica behind a load balancer would answer differently — which is
    /// exactly the bug that statelessness exists to make impossible.
    #[test]
    fn two_servers_answer_identically() {
        let first = pollster::block_on(server().handle(&request(7, "tools/list", Value::Null)));
        let fresh = Server::new("notes-server", "1.0.0", Notes);
        let second = pollster::block_on(fresh.handle(&request(7, "tools/list", Value::Null)));
        assert_eq!(first, second, "any replica may serve any request");
    }

    /// Repeated calls to one instance must also not drift, which is the same property observed from
    /// the other side.
    #[test]
    fn a_server_does_not_accumulate_state_across_calls() {
        let server = server();
        let once = pollster::block_on(server.handle(&request(1, "tools/list", Value::Null)));
        let _ = pollster::block_on(server.handle(&request(
            2,
            "tools/call",
            json!({"name": "note_count"}),
        )));
        let twice = pollster::block_on(server.handle(&request(1, "tools/list", Value::Null)));
        assert_eq!(once, twice);
    }

    #[test]
    fn a_trace_parent_is_echoed_so_spans_join_up() {
        let mut raw = request(1, "tools/list", Value::Null);
        raw["_meta"] = json!({meta_keys::TRACEPARENT: "00-abc-def-01"});
        let reply = call(&raw);
        assert_eq!(reply["result"]["_meta"][meta_keys::TRACEPARENT], "00-abc-def-01");
    }

    /// A pre-stateless client sends `initialize` first and gives up on an error. Answering it is a
    /// deliberate compatibility shim, and this test is what stops it being deleted as dead code.
    #[test]
    fn a_legacy_client_that_sends_initialize_is_still_served() {
        let reply = call(&request(1, "initialize", json!({"protocolVersion": "2025-11-25"})));
        assert!(reply.get("error").is_none(), "{reply}");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn a_request_without_a_method_is_refused_rather_than_ignored() {
        let reply = call(&json!({"jsonrpc": "2.0", "id": 1}));
        assert_eq!(reply["error"]["code"], codes::INVALID_REQUEST);
    }

    /// The test that justifies having built both halves: Frey's own client, unmodified, against
    /// Frey's own server. Each was written against the specification rather than against the other,
    /// so agreement here is evidence about the specification reading rather than about a shared
    /// assumption.
    #[test]
    fn freys_client_and_freys_server_agree_over_a_loopback() {
        use crate::client::{McpClient, Transport, TransportError};
        use crate::protocol::Request as McpRequest;

        struct Loopback(Server<Notes>);

        impl Transport for Loopback {
            async fn send(&self, request: &McpRequest) -> Result<Value, TransportError> {
                let raw = serde_json::to_value(request).expect("a request serialises");
                Ok(self.0.handle(&raw).await.expect("a request with an id gets an answer"))
            }
        }

        let client = McpClient::new("notes-server", Loopback(server()));

        let identity = pollster::block_on(client.negotiate()).expect("negotiation succeeds");
        assert!(identity.stateless, "the client sees a stateless server");
        assert_eq!(identity.protocol_version, PROTOCOL_VERSION);

        let catalog = pollster::block_on(client.list_tools()).expect("the catalog arrives");
        let names: Vec<String> = catalog.tools.iter().map(|t| t.name.to_string()).collect();
        assert!(
            names.iter().any(|n| n.ends_with("note_count")),
            "the client sees the server's tools: {names:?}"
        );
        assert!(!catalog.is_shareable(), "a private listing is not shared between principals");
    }
}
