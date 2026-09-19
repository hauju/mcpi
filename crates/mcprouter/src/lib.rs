//! The gateway half of mcpi: many upstream MCP servers behind two tools.
//!
//! A client connected to the gateway sees `find_tools` and `call_tool` (plus any tools the
//! operator lists directly) instead of every upstream's full definitions. `find_tools` asks
//! TypeSafe's Jev model one `choice` question over the whole catalog and returns the top
//! definitions; `call_tool` proxies to the owning upstream verbatim.
//!
//! Every deterministic decision stays in code: the cumulative-probability selection rule, the
//! `k` bounds, the flat-distribution fallback, the never-empty guarantee, and which tools are
//! exposed directly. The model only ranks.
//!
//! This crate does not know how upstreams are dialled — [`Upstreams`] is the seam. The CLI
//! implements it over `mcpclient` handles; a hosted gateway can implement it over anything.

pub mod bm25;
pub mod gateway;
pub mod jev;
pub mod log;
pub mod router;
pub mod serve;
pub mod stats;

use std::future::Future;
use std::pin::Pin;

use rmcp::model::{CallToolResult, JsonObject, Tool};

pub use gateway::Gateway;
pub use jev::Jev;
pub use router::{Router, RouterSettings, Routing};
pub use stats::{Stats, Thresholds};

/// One routable tool: the upstream's definition plus the name the gateway exposes for it.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Name exposed by the gateway. The implementor of [`Upstreams`] decides how collisions
    /// between servers are resolved (mcpi-cli uses `<server>__<name>`).
    pub key: String,
    /// The upstream server this tool belongs to, as shown to the client.
    pub server: String,
    pub tool: Tool,
}

impl Entry {
    /// `key: description` on one line, whitespace collapsed and truncated, for the routing prompt.
    pub fn oneline(&self, max_chars: usize) -> String {
        let desc: String = self
            .tool
            .description
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let desc = if desc.chars().count() > max_chars {
            let cut: String = desc.chars().take(max_chars.saturating_sub(1)).collect();
            format!("{cut}…")
        } else {
            desc
        };
        if desc.is_empty() {
            self.key.clone()
        } else {
            format!("{}: {desc}", self.key)
        }
    }
}

/// How an upstream's contract moved since the gateway last saw it, from `schemadiff`.
///
/// Surfaced in `find_tools` results so a client learns that a tool it is about to call has a
/// changed contract, rather than discovering it from a failed call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractNote {
    pub server: String,
    pub breaking: usize,
    pub compatible: usize,
    pub cosmetic: usize,
}

impl ContractNote {
    pub fn from_diff(server: impl Into<String>, diff: &schemadiff::SnapshotDiff) -> Self {
        let c = diff.counts();
        Self {
            server: server.into(),
            breaking: c.breaking,
            compatible: c.compatible,
            cosmetic: c.cosmetic,
        }
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The connected upstreams: a catalog to route over and a way to call into it.
pub trait Upstreams: Send + Sync + 'static {
    fn entries(&self) -> &[Entry];

    /// Forward a call to the upstream that owns `key`. `Err` is a transport or protocol failure;
    /// a tool that ran and reported `is_error` is still `Ok`.
    fn call<'a>(
        &'a self,
        key: &'a str,
        arguments: Option<JsonObject>,
    ) -> BoxFuture<'a, Result<CallToolResult, String>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn entry(key: &str, desc: &str) -> Entry {
        Entry {
            key: key.into(),
            server: "s".into(),
            tool: Tool::new(
                key.to_string(),
                desc.to_string(),
                Arc::new(JsonObject::new()),
            ),
        }
    }

    #[test]
    fn oneline_collapses_and_truncates() {
        assert_eq!(entry("t", "a  b\n c").oneline(100), "t: a b c");
        assert_eq!(entry("t", "").oneline(100), "t");
        assert_eq!(entry("t", "abcdefghij").oneline(5), "t: abcd…");
    }
}
