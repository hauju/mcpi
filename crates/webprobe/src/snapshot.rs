//! Turning a page's tool list into a [`Snapshot`].

use std::collections::BTreeMap;

use schemadiff::Snapshot;
use serde_json::{Value, json};

use crate::{Error, Result};

pub(crate) fn build(url: &str, tools: &[Value]) -> Result<Snapshot> {
    let mut keyed = BTreeMap::new();
    for tool in tools {
        let name = tool["name"].as_str().ok_or(Error::NamelessTool)?;
        // The whole object, as with `tools/list` on an MCP server: a field
        // added by a future WebMCP revision then shows up in a diff instead of
        // being quietly dropped by a typed subset.
        keyed.insert(name.to_string(), tool.clone());
    }

    Ok(Snapshot {
        // WebMCP negotiates nothing — the API is whatever the browser ships.
        // Left empty rather than invented so it never moves and never diffs.
        protocol_version: String::new(),
        // The origin, deliberately *not* `document.title`. A title moves with
        // the route and with unread counts ("(3) Inbox — Acme"), so using it
        // would make half the scans of an unchanged page report a change.
        server_name: origin(url),
        server_version: String::new(),
        capabilities: json!({ "tools": {} }),
        tools: keyed,
        // A page has tools and nothing else; these stay empty so that a WebMCP
        // snapshot and an MCP one compare on the axis they share.
        resources: BTreeMap::new(),
        prompts: BTreeMap::new(),
    })
}

fn origin(url: &str) -> String {
    url::Url::parse(url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| url.to_string())
}
