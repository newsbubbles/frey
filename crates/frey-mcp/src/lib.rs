//! Model Context Protocol support for [Frey](https://github.com/newsbubbles/frey).
//!
//! Built for revision `2026-07-28`, which removed the stateful core of the protocol — no handshake,
//! no session id, no SSE resumability — and replaced server-initiated requests with a retry
//! pattern. A client is therefore a **cache over a stateless transport** rather than a session
//! manager, and older servers are handled by a shim that converges on the same internal catalog.
//!
//! An MCP server is an **untrusted party**. Everything it says arrives labelled: tool results are
//! [`frey_core::taint::Untrusted`] by construction, listings are re-sorted defensively so a server
//! cannot churn the prompt cache, and freshness hints are capped.

pub mod client;
pub mod protocol;
pub mod server;

/// The types most callers want.
pub mod prelude {
    pub use crate::client::{
        Catalog, CatalogCache, McpClient, McpError, Transport, TransportError,
    };
    pub use crate::protocol::{
        CacheScope, ClientInfo, McpTool, PROTOCOL_VERSION, Request, ServerIdentity,
    };
    pub use crate::server::{Server, ServerInfo};
}
