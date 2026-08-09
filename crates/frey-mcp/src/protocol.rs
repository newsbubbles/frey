//! The MCP wire format, at revision `2026-07-28`.
//!
//! That revision removed the stateful core of the protocol: no `initialize` handshake, no
//! `Mcp-Session-Id`, no SSE resumability. Every request carries its own protocol version and client
//! capabilities in `_meta`, so any request can land on any server instance behind a plain load
//! balancer. Servers that need cross-call state mint explicit handles and pass them as ordinary
//! tool arguments.
//!
//! Two consequences shape this module:
//!
//! * a client is a **cache over a stateless transport**, not a session manager;
//! * list results carry `ttlMs` and `cacheScope`, so catalogs can be persisted and shared, and the
//!   spec's own recommendation that `tools/list` be deterministically ordered exists specifically
//!   to protect prompt caches.

use serde_json::Value;
use smol_str::SmolStr;

/// The revision this client speaks natively.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// The last revision with the old stateful handshake, which the shim still understands.
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";

/// Well-known `_meta` keys, namespaced by the specification.
pub mod meta_keys {
    /// Which revision the client speaks. Required on every request.
    pub const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
    /// What the client can do. Required on every request.
    pub const CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
    /// Who the client is.
    pub const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
    /// Per-request log level. A server must not emit log notifications without it.
    pub const LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";
    /// W3C trace context, so an agent, its tools, and a remote server share one trace.
    pub const TRACEPARENT: &str = "traceparent";
}

/// How a result should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultType {
    /// An ordinary result.
    #[default]
    Complete,
    /// The server needs something before it can finish.
    InputRequired,
}

/// Who may cache a list result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// Any shared intermediary may cache it.
    Public,
    /// Only this client, for this principal.
    #[default]
    Private,
}

/// A JSON-RPC request, with the `_meta` every stateless request must carry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: SmolStr,
    /// Correlation id.
    pub id: u64,
    /// Which RPC.
    pub method: SmolStr,
    /// Arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Per-request metadata. Not optional in practice: without it a stateless server cannot know
    /// what the client speaks.
    #[serde(rename = "_meta")]
    pub meta: Value,
}

impl Request {
    /// A request carrying the mandatory `_meta` fields.
    #[must_use]
    pub fn new(id: u64, method: &str, params: Option<Value>, client: &ClientInfo) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
            meta: serde_json::json!({
                meta_keys::PROTOCOL_VERSION: PROTOCOL_VERSION,
                meta_keys::CLIENT_CAPABILITIES: client.capabilities,
                meta_keys::CLIENT_INFO: {"name": client.name, "version": client.version},
            }),
        }
    }

    /// Attach a W3C trace parent, so the server's spans join this run's trace.
    #[must_use]
    pub fn with_trace(mut self, traceparent: &str) -> Self {
        self.meta[meta_keys::TRACEPARENT] = Value::String(traceparent.to_string());
        self
    }

    /// The HTTP headers the spec requires on a Streamable HTTP POST.
    ///
    /// These exist so gateways, rate limiters and WAFs can route and meter without parsing the
    /// body, which is why they are mandatory rather than advisory.
    #[must_use]
    pub fn headers(&self) -> Vec<(SmolStr, SmolStr)> {
        let mut headers = vec![("Mcp-Method".into(), self.method.clone())];
        if let Some(name) = self.params.as_ref().and_then(|p| p.get("name")).and_then(Value::as_str)
        {
            headers.push(("Mcp-Name".into(), name.into()));
        }
        headers
    }
}

/// Who the client is, sent on every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// Client name.
    pub name: SmolStr,
    /// Client version.
    pub version: SmolStr,
    /// What it supports.
    pub capabilities: Value,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "frey".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            // Roots, sampling and logging are deprecated in this revision, so a new client should
            // not advertise them. Claiming a deprecated capability invites a server to use it.
            capabilities: serde_json::json!({}),
        }
    }
}

/// What a server said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    /// Server name, if it gave one.
    pub name: Option<SmolStr>,
    /// Which revision this client will use with it.
    pub protocol_version: SmolStr,
    /// Whether it implements the stateless revision.
    pub stateless: bool,
}

/// One tool, as a server describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpTool {
    /// Its name on that server.
    pub name: String,
    /// What it does. **Attacker-controlled text**: a server is an untrusted party, and a
    /// description is the cheapest place to hide an instruction.
    #[serde(default)]
    pub description: String,
    /// Argument schema.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
    /// Result schema, when the server publishes one.
    #[serde(rename = "outputSchema", default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// The result of `tools/list`, including the caching hints this revision requires.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolsList {
    /// The tools.
    #[serde(default)]
    pub tools: Vec<McpTool>,
    /// How long the listing may be considered fresh.
    #[serde(rename = "ttlMs", default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// Who may cache it.
    #[serde(rename = "cacheScope", default)]
    pub cache_scope: CacheScope,
}

/// What a server wants before it can finish, under the multi round-trip pattern.
///
/// The pattern replaced server-initiated requests entirely: rather than the server calling back,
/// it returns this, and the client **retries the original request** with answers attached. That is
/// what allows the protocol to be stateless, and it is the same shape as an A2A `INPUT_REQUIRED`
/// task and an AG-UI interrupt — which is why Frey has one type for all three.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputRequired {
    /// What is needed.
    #[serde(rename = "inputRequests", default)]
    pub input_requests: Vec<Value>,
    /// Opaque state the server needs back on the retry. Sealed by the server; the client must not
    /// interpret or modify it.
    #[serde(rename = "requestState", default, skip_serializing_if = "Option::is_none")]
    pub request_state: Option<Value>,
}

/// A parsed response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reply {
    /// An ordinary result.
    Complete(Value),
    /// The server needs something first.
    InputRequired(InputRequired),
    /// The server reported an error.
    Failed {
        /// JSON-RPC error code.
        code: i64,
        /// What it said.
        message: String,
    },
}

/// A response could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The body was not a JSON-RPC response.
    #[error("not a JSON-RPC response: {0}")]
    Malformed(String),
    /// The server speaks a revision this client cannot.
    #[error(
        "server speaks MCP `{found}`, which this client does not support (it speaks `{}` and can \
         shim `{}`)",
        PROTOCOL_VERSION,
        LEGACY_PROTOCOL_VERSION
    )]
    UnsupportedVersion {
        /// What the server reported.
        found: String,
    },
}

/// Parse a JSON-RPC response body.
///
/// # Errors
/// Returns [`ProtocolError::Malformed`] when the body is not a JSON-RPC response.
pub fn parse_reply(body: &Value) -> Result<Reply, ProtocolError> {
    if let Some(error) = body.get("error") {
        return Ok(Reply::Failed {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string(),
        });
    }

    let result = body
        .get("result")
        .ok_or_else(|| ProtocolError::Malformed("no `result` and no `error`".into()))?;

    // A result from an older server omits `resultType`, and the specification says to treat that
    // as `complete`.
    match result.get("resultType").and_then(Value::as_str) {
        Some("input_required") => {
            let parsed: InputRequired = serde_json::from_value(result.clone())
                .map_err(|e| ProtocolError::Malformed(e.to_string()))?;
            Ok(Reply::InputRequired(parsed))
        }
        _ => Ok(Reply::Complete(result.clone())),
    }
}

/// Whether a JSON-RPC error code means "this server does not implement that method".
///
/// Used to probe for `server/discover`: a server that predates the stateless revision answers
/// method-not-found, which is a negotiation signal rather than a failure.
#[must_use]
pub fn is_method_not_found(code: i64) -> bool {
    code == -32601
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_carries_the_metadata_a_stateless_server_needs() {
        // There is no handshake any more, so a request that omits this is unanswerable.
        let request = Request::new(1, "tools/list", None, &ClientInfo::default());
        assert_eq!(request.meta[meta_keys::PROTOCOL_VERSION], "2026-07-28");
        assert!(request.meta.get(meta_keys::CLIENT_CAPABILITIES).is_some());
        assert_eq!(request.meta[meta_keys::CLIENT_INFO]["name"], "frey");
    }

    #[test]
    fn deprecated_capabilities_are_not_advertised() {
        // Roots, sampling and logging are deprecated in this revision. Advertising them invites a
        // server to use a feature that is scheduled for removal.
        let capabilities = ClientInfo::default().capabilities;
        for deprecated in ["roots", "sampling", "logging"] {
            assert!(capabilities.get(deprecated).is_none(), "must not claim {deprecated}");
        }
    }

    #[test]
    fn routing_headers_let_a_gateway_meter_without_parsing_the_body() {
        let request = Request::new(
            1,
            "tools/call",
            Some(serde_json::json!({"name": "fs_read", "arguments": {}})),
            &ClientInfo::default(),
        );
        let headers = request.headers();
        assert!(headers.contains(&("Mcp-Method".into(), "tools/call".into())));
        assert!(headers.contains(&("Mcp-Name".into(), "fs_read".into())));
    }

    #[test]
    fn a_trace_parent_joins_the_servers_spans_to_this_run() {
        let request =
            Request::new(1, "tools/list", None, &ClientInfo::default()).with_trace("00-abc-def-01");
        assert_eq!(request.meta[meta_keys::TRACEPARENT], "00-abc-def-01");
    }

    #[test]
    fn a_result_without_a_result_type_is_treated_as_complete() {
        // Required for older servers, and stated as such in the specification.
        let reply = parse_reply(&serde_json::json!({"result": {"tools": []}})).unwrap();
        assert!(matches!(reply, Reply::Complete(_)));
    }

    #[test]
    fn an_input_required_result_is_recognised_and_keeps_the_servers_sealed_state() {
        let reply = parse_reply(&serde_json::json!({
            "result": {
                "resultType": "input_required",
                "inputRequests": [{"kind": "confirm", "prompt": "delete?"}],
                "requestState": {"sealed": "opaque-hmac"}
            }
        }))
        .unwrap();

        let Reply::InputRequired(input) = reply else { panic!("expected input_required") };
        assert_eq!(input.input_requests.len(), 1);
        // The client must hand this back untouched; interpreting it would break the server's seal.
        assert_eq!(input.request_state.unwrap()["sealed"], "opaque-hmac");
    }

    #[test]
    fn errors_are_parsed_rather_than_thrown_away() {
        let reply = parse_reply(
            &serde_json::json!({"error": {"code": -32601, "message": "no such method"}}),
        )
        .unwrap();
        let Reply::Failed { code, message } = reply else { panic!("expected a failure") };
        assert!(is_method_not_found(code), "which is how discovery probing works");
        assert_eq!(message, "no such method");
    }

    #[test]
    fn a_body_that_is_neither_result_nor_error_is_malformed() {
        assert!(matches!(
            parse_reply(&serde_json::json!({"jsonrpc": "2.0"})),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn list_results_carry_the_caching_hints_the_revision_requires() {
        let list: ToolsList = serde_json::from_value(serde_json::json!({
            "tools": [{"name": "fs_read", "description": "reads", "inputSchema": {"type": "object"}}],
            "ttlMs": 60000,
            "cacheScope": "public"
        }))
        .unwrap();
        assert_eq!(list.ttl_ms, Some(60_000));
        assert_eq!(list.cache_scope, CacheScope::Public);
    }

    #[test]
    fn a_listing_without_hints_defaults_to_private() {
        // The conservative default: sharing a catalog across principals when the server did not say
        // it was safe would leak one user's tools to another.
        let list: ToolsList = serde_json::from_value(serde_json::json!({"tools": []})).unwrap();
        assert_eq!(list.cache_scope, CacheScope::Private);
        assert_eq!(list.ttl_ms, None);
    }
}
