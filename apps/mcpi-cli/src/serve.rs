//! `mcpi-cli serve` — the saved server library as one MCP gateway.
//!
//! Every row the desktop app knows is dialled the way the app dials it (same transport config,
//! same keychain credentials), snapshotted so the contract history keeps flowing, and put behind
//! `find_tools` / `call_tool`. A client that connects to this process sees two tools instead of
//! every upstream's full definitions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mcpclient::{Handle, Transport};
use mcprouter::{BoxFuture, ContractNote, Entry, Upstreams};
use mcpstore::{ServerId, ServerRow, SnapshotOutcome, Store, TransportKind};
use rmcp::model::{CallToolResult, JsonObject};
use serde::Deserialize;

/// Mirrors the desktop app's stored transport config. The store keeps it as opaque JSON; this
/// and `apps/mcpi/src/config.rs` are the two readers, and they must agree.
#[derive(Debug, Deserialize)]
struct StdioConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpConfig {
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// The keychain account the app stores a server's OAuth credentials under — reusing it is what
/// lets the CLI ride a session the app already authorised.
fn credential_key(server_id: ServerId) -> String {
    format!("server-{server_id}")
}

fn to_transport(row: &ServerRow) -> Result<Option<Transport>, String> {
    let bad = |e: serde_json::Error| format!("`{}` has an unreadable config: {e}", row.name);
    Ok(Some(match row.transport_kind {
        TransportKind::Stdio => {
            let c: StdioConfig = serde_json::from_value(row.config.clone()).map_err(bad)?;
            Transport::Stdio {
                command: c.command,
                args: c.args,
                env: c.env,
                cwd: c.cwd.map(PathBuf::from),
            }
        }
        TransportKind::Http => {
            let c: HttpConfig = serde_json::from_value(row.config.clone()).map_err(bad)?;
            Transport::Http {
                url: c.url,
                headers: c.headers,
                credential_key: Some(credential_key(row.id)),
            }
        }
        TransportKind::WebMcp => return Ok(None),
    }))
}

/// A server name as a tool-name prefix: MCP tool names are `[A-Za-z0-9_-]`.
fn prefix(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The live sessions behind the gateway.
pub struct Live {
    entries: Vec<Entry>,
    handles: HashMap<String, Handle>,
    /// Upstreams whose contract moved since the app last saw them.
    pub notes: Vec<ContractNote>,
}

impl Live {
    /// Dial every dialable row in `store` (or only `only`, by name), snapshot each, and build the
    /// catalog. Rows that fail to connect are reported and skipped rather than failing the whole
    /// gateway — one broken upstream should not take the other nine offline.
    pub async fn connect(store: &Store, only: &[String]) -> Result<Self, String> {
        let rows = store
            .list_servers()
            .map_err(|e| format!("could not list servers: {e}"))?;
        let rows: Vec<ServerRow> = rows
            .into_iter()
            .filter(|r| only.is_empty() || only.iter().any(|n| n == &r.name))
            .collect();
        if rows.is_empty() {
            return Err(if only.is_empty() {
                "the store has no servers. Add some in the mcpi app first.".into()
            } else {
                format!("no saved server named {}", only.join(", "))
            });
        }

        let mut raw: Vec<(String, rmcp::model::Tool)> = Vec::new();
        let mut handles = HashMap::new();
        let mut notes = Vec::new();
        for row in &rows {
            let Some(transport) = to_transport(row)? else {
                eprintln!("skipping `{}`: a WebMCP page is read, not called", row.name);
                continue;
            };
            let handle = match Handle::connect(&transport).await {
                Ok((h, _)) => h,
                Err(mcpclient::Error::AuthRequired { .. }) => {
                    eprintln!("skipping `{}`: sign in from the mcpi app first", row.name);
                    continue;
                }
                Err(e) => {
                    eprintln!("skipping `{}`: {e}", row.name);
                    continue;
                }
            };
            match handle.snapshot().await {
                Ok(snapshot) => match store.record_snapshot(row.id, &snapshot) {
                    Ok(SnapshotOutcome::Changed { diff, .. }) => {
                        notes.push(ContractNote::from_diff(&row.name, &diff));
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("`{}`: could not record its snapshot: {e}", row.name),
                },
                Err(e) => eprintln!("`{}`: snapshot failed: {e}", row.name),
            }
            let _ = store.mark_connected(row.id);
            let tools = handle
                .list_tools()
                .await
                .map_err(|e| format!("`{}`: tools/list failed: {e}", row.name))?;
            eprintln!("connected `{}`: {} tools", row.name, tools.len());
            raw.extend(tools.into_iter().map(|t| (row.name.clone(), t)));
            handles.insert(row.name.clone(), handle);
        }
        if handles.is_empty() {
            return Err("no upstream could be connected".into());
        }

        let mut count: HashMap<String, usize> = HashMap::new();
        for (_, t) in &raw {
            *count.entry(t.name.to_string()).or_default() += 1;
        }
        let entries = raw
            .into_iter()
            .map(|(server, tool)| {
                let key = if count[tool.name.as_ref()] > 1 {
                    format!("{}__{}", prefix(&server), tool.name)
                } else {
                    tool.name.to_string()
                };
                Entry { key, server, tool }
            })
            .collect();
        Ok(Self {
            entries,
            handles,
            notes,
        })
    }
}

impl Upstreams for Live {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn call<'a>(
        &'a self,
        key: &'a str,
        arguments: Option<JsonObject>,
    ) -> BoxFuture<'a, Result<CallToolResult, String>> {
        Box::pin(async move {
            let entry = self
                .entries
                .iter()
                .find(|e| e.key == key)
                .ok_or_else(|| format!("unknown tool {key}"))?;
            let handle = self
                .handles
                .get(&entry.server)
                .ok_or_else(|| format!("no session for {}", entry.server))?;
            handle
                .call_tool(&entry.tool.name, arguments.unwrap_or_default())
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Where `serve` logs by default: next to the store, so the app can read it later.
pub fn default_log_path(store: &Path) -> PathBuf {
    store
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("router.jsonl")
}

/// Read `key` from a `KEY=value` `.env` in the current directory, for local runs.
pub fn dotenv_value(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(".env").ok()?;
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
        })
}

pub fn shared(live: Live) -> Arc<dyn Upstreams> {
    Arc::new(live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_keeps_tool_name_charset() {
        assert_eq!(prefix("my server (prod)"), "my_server__prod_");
        assert_eq!(prefix("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn transports_mirror_the_app() {
        let row = ServerRow {
            id: 7,
            name: "x".into(),
            transport_kind: TransportKind::Http,
            config: serde_json::json!({"url": "https://h/mcp", "headers": {"A": "b"}}),
            created_at: chrono::Utc::now(),
            last_connected_at: None,
            group_id: None,
        };
        match to_transport(&row).unwrap().unwrap() {
            Transport::Http {
                url,
                headers,
                credential_key,
            } => {
                assert_eq!(url, "https://h/mcp");
                assert_eq!(headers["A"], "b");
                assert_eq!(credential_key.as_deref(), Some("server-7"));
            }
            other => panic!("{other:?}"),
        }
    }
}
