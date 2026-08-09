//! The MCP client: a cache over a stateless transport.
//!
//! Because the protocol no longer has sessions, a client's real job is caching and defence rather
//! than connection management:
//!
//! * **Negotiate once.** Probe `server/discover`; a method-not-found means an older server, and the
//!   shim takes over. Both paths converge on one internal catalog.
//! * **Cache the catalog**, honouring `ttlMs` but capping it, because the freshness hint comes from
//!   an untrusted party.
//! * **Re-sort defensively.** A server that reorders its listing would otherwise rewrite the tool
//!   block's hash every turn and silently destroy the prompt cache.
//! * **Label everything it says.** A tool description is attacker-controlled text; it reaches a
//!   prompt as low-integrity data with its provenance attached.

use std::collections::BTreeMap;

use frey_core::ids::{ServerId, ToolName};
use frey_core::taint::{Provenance, Tainted, Untrusted};
use frey_core::tool_def::{JsonSchema, ToolDefinition};
use serde_json::Value;

use crate::protocol::{
    CacheScope, ClientInfo, LEGACY_PROTOCOL_VERSION, McpTool, PROTOCOL_VERSION, ProtocolError,
    Reply, Request, ServerIdentity, ToolsList, is_method_not_found, parse_reply,
};

/// Somewhere to send a request. Abstracted so the whole client is testable without a server.
pub trait Transport: Send + Sync {
    /// Send one request and return the parsed body.
    fn send(&self, request: &Request)
    -> impl Future<Output = Result<Value, TransportError>> + Send;
}

/// The request could not be delivered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transport failure talking to `{server}`: {detail}")]
pub struct TransportError {
    /// Which server.
    pub server: ServerId,
    /// What went wrong.
    pub detail: String,
}

/// Something went wrong talking to a server.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// The request could not be delivered.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The response could not be understood.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// The server reported an error.
    #[error("`{server}` reported error {code}: {message}")]
    Server {
        /// Which server.
        server: ServerId,
        /// JSON-RPC code.
        code: i64,
        /// What it said.
        message: String,
    },
    /// The server needs something before it can answer.
    #[error("`{server}` needs input before it can complete this call")]
    NeedsInput {
        /// Which server.
        server: ServerId,
        /// What it asked for, and the sealed state to hand back on the retry.
        required: Box<crate::protocol::InputRequired>,
    },
}

/// A cached tool catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// Which server it came from.
    pub server: ServerId,
    /// The tools, in a deterministic order.
    pub tools: Vec<ToolDefinition>,
    /// How long the server said it stays fresh, after capping.
    pub ttl_ms: Option<u64>,
    /// Whether it may be shared across principals.
    pub scope: CacheScope,
}

impl Catalog {
    /// Whether this catalog may be shared with a different principal.
    #[must_use]
    pub fn is_shareable(&self) -> bool {
        self.scope == CacheScope::Public
    }
}

/// No server's freshness hint is honoured beyond this. A server is an untrusted party, and a
/// `ttlMs` of a year would pin a stale catalog indefinitely.
pub const MAX_TTL_MS: u64 = 60 * 60 * 1000;

/// A client for one MCP server.
#[derive(Debug)]
pub struct McpClient<T> {
    server: ServerId,
    transport: T,
    client_info: ClientInfo,
    next_id: std::sync::atomic::AtomicU64,
}

impl<T: Transport> McpClient<T> {
    /// A client for `server`.
    pub fn new(server: impl Into<ServerId>, transport: T) -> Self {
        Self {
            server: server.into(),
            transport,
            client_info: ClientInfo::default(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let request = Request::new(self.next_id(), method, params, &self.client_info);
        let body = self.transport.send(&request).await?;
        match parse_reply(&body)? {
            Reply::Complete(result) => Ok(result),
            Reply::Failed { code, message } => {
                Err(McpError::Server { server: self.server.clone(), code, message })
            }
            Reply::InputRequired(required) => Err(McpError::NeedsInput {
                server: self.server.clone(),
                required: Box::new(required),
            }),
        }
    }

    /// Work out which revision this server speaks.
    ///
    /// A server implementing the stateless revision answers `server/discover`. One that does not
    /// answers method-not-found, which is negotiation rather than failure — so the shim takes over
    /// and both paths converge on the same internal catalog.
    ///
    /// # Errors
    /// Returns [`McpError`] for transport or protocol failures. A method-not-found is not a failure.
    pub async fn negotiate(&self) -> Result<ServerIdentity, McpError> {
        match self.call("server/discover", None).await {
            Ok(result) => Ok(ServerIdentity {
                name: result
                    .get("serverInfo")
                    .and_then(|s| s.get("name"))
                    .and_then(Value::as_str)
                    .map(Into::into),
                protocol_version: PROTOCOL_VERSION.into(),
                stateless: true,
            }),
            Err(McpError::Server { code, .. }) if is_method_not_found(code) => Ok(ServerIdentity {
                name: None,
                protocol_version: LEGACY_PROTOCOL_VERSION.into(),
                stateless: false,
            }),
            Err(other) => Err(other),
        }
    }

    /// Fetch the tool catalog.
    ///
    /// # Errors
    /// Returns [`McpError`] when the listing cannot be fetched or understood.
    pub async fn list_tools(&self) -> Result<Catalog, McpError> {
        let result = self.call("tools/list", None).await?;
        let list: ToolsList =
            serde_json::from_value(result).map_err(|e| ProtocolError::Malformed(e.to_string()))?;

        let mut tools: Vec<ToolDefinition> =
            list.tools.iter().map(|t| to_definition(&self.server, t)).collect();

        // Defensive re-sort. The specification asks servers to be deterministic precisely to
        // protect prompt caches; a server that ignores that would otherwise churn the tool block's
        // hash every turn, and the cost lands on the client.
        tools.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        tools.dedup_by(|a, b| a.name == b.name);

        Ok(Catalog {
            server: self.server.clone(),
            tools,
            ttl_ms: list.ttl_ms.map(|ms| ms.min(MAX_TTL_MS)),
            scope: list.cache_scope,
        })
    }

    /// Call a tool.
    ///
    /// The result is [`Untrusted`] by construction: it came from a party this process does not
    /// control, and there is no way to obtain it otherwise.
    ///
    /// # Errors
    /// Returns [`McpError::NeedsInput`] when the server needs something first, which the caller
    /// projects onto its own approval or elicitation path and then retries.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Untrusted<String>, McpError> {
        let result = self
            .call("tools/call", Some(serde_json::json!({"name": name, "arguments": arguments})))
            .await?;

        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| result.to_string());

        Ok(Tainted::with_provenance(text, Provenance::new(format!("mcp:{}/{name}", self.server))))
    }
}

/// Turn a server's tool into a Frey definition, namespaced by server.
///
/// Namespacing is not cosmetic: it prevents two servers from colliding on a common name like
/// `search`, and it lets one search match a whole server's tools at once.
fn to_definition(server: &ServerId, tool: &McpTool) -> ToolDefinition {
    let schema =
        JsonSchema::new(tool.input_schema.clone()).unwrap_or_else(|_| JsonSchema::empty_object());
    let mut def = ToolDefinition::new(
        ToolName::new(format!("{server}_{}", tool.name)),
        tool.description.clone(),
        schema,
    );
    def.output_schema = tool.output_schema.clone().and_then(|s| JsonSchema::new(s).ok());
    def
}

/// Everything the agent knows from every MCP server, cached.
#[derive(Debug, Clone, Default)]
pub struct CatalogCache {
    entries: BTreeMap<ServerId, Catalog>,
}

impl CatalogCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a catalog.
    pub fn insert(&mut self, catalog: Catalog) {
        self.entries.insert(catalog.server.clone(), catalog);
    }

    /// Every tool from every server, in a deterministic order.
    ///
    /// `BTreeMap` iteration plus the per-server sort means this is stable across processes, which
    /// is what keeps the tool block's hash — and therefore the prompt cache — stable across
    /// restarts.
    #[must_use]
    pub fn all_tools(&self) -> Vec<ToolDefinition> {
        self.entries.values().flat_map(|c| c.tools.iter().cloned()).collect()
    }

    /// How many servers are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The catalogs that may be shared with another principal.
    pub fn shareable(&self) -> impl Iterator<Item = &Catalog> {
        self.entries.values().filter(|c| c.is_shareable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A server that answers from a script and records what it was asked.
    struct FakeServer {
        replies: Mutex<Vec<Value>>,
        seen: Mutex<Vec<Request>>,
    }

    impl FakeServer {
        fn new(replies: Vec<Value>) -> Self {
            Self { replies: Mutex::new(replies), seen: Mutex::new(Vec::new()) }
        }

        fn seen(&self) -> Vec<Request> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Transport for &FakeServer {
        async fn send(&self, request: &Request) -> Result<Value, TransportError> {
            self.seen.lock().unwrap().push(request.clone());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Err(TransportError {
                    server: ServerId::new("fake"),
                    detail: "the script ran out".into(),
                });
            }
            Ok(replies.remove(0))
        }
    }

    fn listing(tools: Value, extra: Value) -> Value {
        let mut result = serde_json::json!({"tools": tools});
        if let (Some(r), Some(e)) = (result.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                r.insert(k.clone(), v.clone());
            }
        }
        serde_json::json!({"result": result})
    }

    #[test]
    fn a_modern_server_is_recognised_from_server_discover() {
        let server = FakeServer::new(vec![
            serde_json::json!({"result": {"serverInfo": {"name": "github"}}}),
        ]);
        let client = McpClient::new("github", &server);
        let identity = pollster::block_on(client.negotiate()).unwrap();

        assert!(identity.stateless);
        assert_eq!(identity.protocol_version, PROTOCOL_VERSION);
        assert_eq!(identity.name.as_deref(), Some("github"));
    }

    #[test]
    fn an_older_server_is_negotiated_rather_than_failed() {
        // A method-not-found for `server/discover` is how a pre-stateless server identifies itself.
        // Treating it as an error would make every existing server unusable.
        let server = FakeServer::new(vec![
            serde_json::json!({"error": {"code": -32601, "message": "unknown method"}}),
        ]);
        let client = McpClient::new("legacy", &server);
        let identity = pollster::block_on(client.negotiate()).unwrap();

        assert!(!identity.stateless);
        assert_eq!(identity.protocol_version, LEGACY_PROTOCOL_VERSION);
    }

    #[test]
    fn a_real_server_error_is_still_an_error() {
        let server = FakeServer::new(vec![
            serde_json::json!({"error": {"code": -32000, "message": "internal"}}),
        ]);
        let client = McpClient::new("broken", &server);
        assert!(matches!(
            pollster::block_on(client.negotiate()),
            Err(McpError::Server { code: -32000, .. })
        ));
    }

    #[test]
    fn a_server_that_reorders_its_listing_cannot_churn_the_tool_block() {
        // Left alone this rewrites the prefix hash every turn and destroys the prompt cache
        // silently. The cost would land on the client, so the client defends itself.
        let unsorted = serde_json::json!([
            {"name": "z_last", "description": "last", "inputSchema": {"type": "object"}},
            {"name": "a_first", "description": "first", "inputSchema": {"type": "object"}}
        ]);
        let server = FakeServer::new(vec![listing(unsorted, serde_json::json!({}))]);
        let catalog = pollster::block_on(McpClient::new("gh", &server).list_tools()).unwrap();

        let names: Vec<&str> = catalog.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["gh_a_first", "gh_z_last"]);
    }

    #[test]
    fn tools_are_namespaced_by_server_so_two_servers_cannot_collide() {
        let tools = serde_json::json!([
            {"name": "search", "description": "search things", "inputSchema": {"type": "object"}}
        ]);
        let a = FakeServer::new(vec![listing(tools.clone(), serde_json::json!({}))]);
        let b = FakeServer::new(vec![listing(tools, serde_json::json!({}))]);

        let cat_a = pollster::block_on(McpClient::new("github", &a).list_tools()).unwrap();
        let cat_b = pollster::block_on(McpClient::new("slack", &b).list_tools()).unwrap();

        assert_eq!(cat_a.tools[0].name.as_str(), "github_search");
        assert_eq!(cat_b.tools[0].name.as_str(), "slack_search");
        assert_eq!(cat_a.tools[0].namespace(), Some("github"));
    }

    #[test]
    fn an_absurd_freshness_hint_is_capped() {
        // The hint comes from an untrusted party. A year-long ttl would pin a stale catalog.
        let server = FakeServer::new(vec![listing(
            serde_json::json!([]),
            serde_json::json!({"ttlMs": 31_536_000_000u64}),
        )]);
        let catalog = pollster::block_on(McpClient::new("gh", &server).list_tools()).unwrap();
        assert_eq!(catalog.ttl_ms, Some(MAX_TTL_MS));
    }

    #[test]
    fn a_catalog_is_private_unless_the_server_says_otherwise() {
        let server = FakeServer::new(vec![listing(serde_json::json!([]), serde_json::json!({}))]);
        let catalog = pollster::block_on(McpClient::new("gh", &server).list_tools()).unwrap();
        assert!(
            !catalog.is_shareable(),
            "sharing by default would leak one user's tools to another"
        );
    }

    #[test]
    fn a_tool_result_is_untrusted_with_its_origin_recorded() {
        let server = FakeServer::new(vec![
            serde_json::json!({"result": {"content": [{"type": "text", "text": "file body"}]}}),
        ]);
        let client = McpClient::new("files", &server);
        let value = pollster::block_on(client.call_tool("read", serde_json::json!({}))).unwrap();

        assert_eq!(value.peek(), "file body");
        assert_eq!(value.label().0, frey_core::taint::IntegrityLevel::Low);
        assert_eq!(value.provenance().origin.as_str(), "mcp:files/read");
    }

    #[test]
    fn an_input_required_reply_surfaces_the_servers_sealed_state_for_the_retry() {
        let server = FakeServer::new(vec![serde_json::json!({
            "result": {
                "resultType": "input_required",
                "inputRequests": [{"kind": "confirm"}],
                "requestState": {"sealed": "hmac"}
            }
        })]);
        let client = McpClient::new("files", &server);
        let err =
            pollster::block_on(client.call_tool("delete", serde_json::json!({}))).unwrap_err();

        let McpError::NeedsInput { required, .. } = err else { panic!("expected NeedsInput") };
        assert_eq!(required.request_state.unwrap()["sealed"], "hmac");
    }

    #[test]
    fn every_request_carries_routing_headers_and_metadata() {
        let server = FakeServer::new(vec![listing(serde_json::json!([]), serde_json::json!({}))]);
        pollster::block_on(McpClient::new("gh", &server).list_tools()).unwrap();

        let request = &server.seen()[0];
        assert_eq!(request.method, "tools/list");
        assert!(request.headers().iter().any(|(k, _)| k == "Mcp-Method"));
        assert_eq!(request.meta[crate::protocol::meta_keys::PROTOCOL_VERSION], PROTOCOL_VERSION);
    }

    #[test]
    fn the_combined_catalog_is_ordered_the_same_way_on_every_process() {
        // A HashMap here would make the tool block's hash depend on a per-process seed, so every
        // restart would pay to rebuild the prompt cache.
        let mut cache = CatalogCache::new();
        for server in ["slack", "github"] {
            cache.insert(Catalog {
                server: ServerId::new(server),
                tools: vec![ToolDefinition::new(
                    format!("{server}_search"),
                    "search this service for matching records",
                    JsonSchema::empty_object(),
                )],
                ttl_ms: None,
                scope: CacheScope::Private,
            });
        }
        let names: Vec<String> = cache.all_tools().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["github_search", "slack_search"]);
    }
}
