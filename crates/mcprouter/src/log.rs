//! Append-only JSONL log of routing decisions and proxied calls (fine-tune / eval data).

use std::path::Path;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::ContractNote;
use crate::router::Routing;

#[derive(Serialize)]
pub struct CatalogEntry<'a> {
    pub key: &'a str,
    pub server: &'a str,
    /// Size of the compact JSON tool definition (name + description + inputSchema) in characters;
    /// divide by ~4 for a token estimate.
    pub def_chars: usize,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Record<'a> {
    /// Written once per start: what the gateway is routing over.
    Catalog {
        ts: String,
        tools: Vec<CatalogEntry<'a>>,
        expose_direct: &'a [String],
        /// Upstreams whose contract moved since the last snapshot, per `schemadiff`.
        contract_changes: &'a [ContractNote],
    },
    FindTools {
        ts: String,
        session: &'a str,
        find_id: u64,
        request: &'a str,
        k: usize,
        #[serde(flatten)]
        routing: &'a Routing,
        returned: Vec<&'a str>,
    },
    CallTool {
        ts: String,
        session: &'a str,
        name: &'a str,
        server: Option<&'a str>,
        /// True when the client called the tool directly (listed via `expose_direct`).
        direct: bool,
        /// The most recent `find_tools` in this session, and where this tool ranked in it.
        find_id: Option<u64>,
        rank: Option<usize>,
        p: Option<f64>,
        in_returned: Option<bool>,
        ok: bool,
        is_error: bool,
        latency_ms: u64,
    },
}

pub struct JsonlLog {
    file: Mutex<tokio::fs::File>,
}

impl JsonlLog {
    pub async fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        if let Some(dir) = path.as_ref().parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub async fn write(&self, record: &Record<'_>) {
        let mut line = match serde_json::to_string(record) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "could not serialise log record");
                return;
            }
        };
        line.push('\n');
        let mut f = self.file.lock().await;
        if let Err(e) = f.write_all(line.as_bytes()).await {
            tracing::warn!(error = %e, "could not write log record");
        }
    }
}

pub fn now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}
