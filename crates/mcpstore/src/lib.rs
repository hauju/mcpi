//! The local store: saved servers, capability snapshots, and call history.
//!
//! This is the whole reason the product can do things the official inspector
//! cannot. Schema diffing needs yesterday's schema; replay needs yesterday's
//! call; "just connect" needs yesterday's credentials.
//!
//! Two decisions worth knowing:
//!
//! - **Secrets never land in SQLite.** API keys and OAuth tokens go to the
//!   macOS Keychain via [`secrets`], keyed by server id. The database records
//!   only that a secret exists.
//! - **Snapshots are content-addressed.** [`Store::record_snapshot`] hashes the
//!   canonical form and writes a row only when it differs from the server's
//!   most recent one, so reconnecting fifty times a day leaves one row per
//!   actual change — and the return value tells the caller what moved.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use schemadiff::{Snapshot, SnapshotDiff};

mod schema;
pub mod secrets;
#[cfg(test)]
mod tests;

pub type ServerId = i64;
pub type SnapshotId = i64;
pub type CallId = i64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("stored JSON could not be read back: {0}")]
    Json(#[from] serde_json::Error),

    #[error("no server with id {0}")]
    UnknownServer(ServerId),

    #[error("keychain error: {0}")]
    Keyring(#[from] keyring::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How a server is reached. Mirrors `mcpclient::Transport` without depending on
/// it — the store has no business knowing how to talk MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
}

impl TransportKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "http" => Self::Http,
            _ => Self::Stdio,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerRow {
    pub id: ServerId,
    pub name: String,
    pub transport_kind: TransportKind,
    /// Transport configuration, opaque to the store. Must not contain secrets.
    pub config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewServer {
    pub name: String,
    pub transport_kind: TransportKind,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredSnapshot {
    pub id: SnapshotId,
    pub server_id: ServerId,
    pub taken_at: DateTime<Utc>,
    pub digest: String,
    /// Human name pinned on this snapshot ("v1.2"), unique per server.
    pub label: Option<String>,
    pub snapshot: Snapshot,
}

/// What [`Store::record_snapshot`] found when it compared against history.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotOutcome {
    /// First time we have seen this server. Nothing to compare against.
    First { id: SnapshotId },
    /// Byte-identical to the last recorded snapshot; no row was written.
    Unchanged { id: SnapshotId },
    /// The contract moved. `diff` is what changed and how badly.
    Changed {
        id: SnapshotId,
        previous: SnapshotId,
        diff: Box<SnapshotDiff>,
    },
}

impl SnapshotOutcome {
    pub fn id(&self) -> SnapshotId {
        match self {
            Self::First { id } | Self::Unchanged { id } | Self::Changed { id, .. } => *id,
        }
    }

    pub fn diff(&self) -> Option<&SnapshotDiff> {
        match self {
            Self::Changed { diff, .. } => Some(diff),
            _ => None,
        }
    }
}

/// One thing sent to a server, and what came back.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallKind {
    Tool,
    Resource,
    Prompt,
}

impl CallKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Resource => "resource",
            Self::Prompt => "prompt",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "resource" => Self::Resource,
            "prompt" => Self::Prompt,
            _ => Self::Tool,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewCall {
    pub server_id: ServerId,
    pub kind: CallKind,
    /// Tool name, resource URI, or prompt name.
    pub target: String,
    pub request: serde_json::Value,
    pub response: serde_json::Value,
    pub is_error: bool,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallRow {
    pub id: CallId,
    pub server_id: ServerId,
    pub kind: CallKind,
    pub target: String,
    pub request: serde_json::Value,
    pub response: serde_json::Value,
    pub is_error: bool,
    pub duration_ms: i64,
    pub started_at: DateTime<Utc>,
}

/// Handle to the local database.
///
/// A plain `Mutex<Connection>` rather than a pool: this is a single-user
/// desktop app and local SQLite reads are microseconds, so a pool would add
/// moving parts to solve a problem that does not exist.
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

/// Where the desktop app keeps its database:
/// `~/Library/Application Support/mcpi/store.db` on macOS.
///
/// Lives here so every binary that opens the store — the app and the CLI —
/// derives the same location from the same code.
pub fn default_path() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("mcpi")
        .join("store.db")
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// An ephemeral store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // Foreign keys are off by default in SQLite, which would let a deleted
        // server leave its snapshots and history behind forever.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock means a bug we cannot recover from by
        // continuing with a half-written transaction.
        self.conn.lock().expect("store mutex poisoned")
    }

    // ── Servers ─────────────────────────────────────────────────────────────

    pub fn add_server(&self, new: NewServer) -> Result<ServerId> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO servers (name, transport_kind, config_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                new.name,
                new.transport_kind.as_str(),
                new.config.to_string(),
                now(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_servers(&self) -> Result<Vec<ServerRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, transport_kind, config_json, created_at, last_connected_at
             FROM servers ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        rows.into_iter()
            .map(|(id, name, kind, config, created, connected)| {
                Ok(ServerRow {
                    id,
                    name,
                    transport_kind: TransportKind::parse(&kind),
                    config: serde_json::from_str(&config)?,
                    created_at: parse_time(&created),
                    last_connected_at: connected.as_deref().map(parse_time),
                })
            })
            .collect()
    }

    pub fn get_server(&self, id: ServerId) -> Result<ServerRow> {
        self.list_servers()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or(Error::UnknownServer(id))
    }

    pub fn update_server(
        &self,
        id: ServerId,
        name: &str,
        config: &serde_json::Value,
    ) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE servers SET name = ?1, config_json = ?2 WHERE id = ?3",
            params![name, config.to_string(), id],
        )?;
        if changed == 0 {
            return Err(Error::UnknownServer(id));
        }
        Ok(())
    }

    /// Removes the server and, by cascade, its snapshots and call history.
    pub fn delete_server(&self, id: ServerId) -> Result<()> {
        let changed = self
            .conn()
            .execute("DELETE FROM servers WHERE id = ?1", params![id])?;
        if changed == 0 {
            return Err(Error::UnknownServer(id));
        }
        // Best-effort: a leftover keychain entry is harmless but untidy.
        let _ = secrets::delete(id);
        Ok(())
    }

    pub fn mark_connected(&self, id: ServerId) -> Result<()> {
        self.conn().execute(
            "UPDATE servers SET last_connected_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
        Ok(())
    }

    // ── Snapshots ───────────────────────────────────────────────────────────

    /// Record what a server just advertised, and say what changed.
    ///
    /// Writes nothing when the snapshot is identical to the last one for this
    /// server, which is the common case on reconnect.
    pub fn record_snapshot(
        &self,
        server_id: ServerId,
        snapshot: &Snapshot,
    ) -> Result<SnapshotOutcome> {
        let digest = blake3::hash(snapshot.digest_source().as_bytes())
            .to_hex()
            .to_string();
        let previous = self.latest_snapshot(server_id)?;

        if let Some(previous) = &previous
            && previous.digest == digest
        {
            return Ok(SnapshotOutcome::Unchanged { id: previous.id });
        }

        let conn = self.conn();
        conn.execute(
            "INSERT INTO snapshots (server_id, taken_at, digest, snapshot_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![server_id, now(), digest, serde_json::to_string(snapshot)?,],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);

        Ok(match previous {
            None => SnapshotOutcome::First { id },
            Some(previous) => SnapshotOutcome::Changed {
                id,
                previous: previous.id,
                diff: Box::new(schemadiff::diff(&previous.snapshot, snapshot)),
            },
        })
    }

    pub fn latest_snapshot(&self, server_id: ServerId) -> Result<Option<StoredSnapshot>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT id, server_id, taken_at, digest, label, snapshot_json FROM snapshots
                 WHERE server_id = ?1 ORDER BY id DESC LIMIT 1",
                params![server_id],
                snapshot_row,
            )
            .optional()?;
        row.map(build_snapshot).transpose()
    }

    /// Snapshot history, newest first.
    pub fn snapshots(&self, server_id: ServerId, limit: usize) -> Result<Vec<StoredSnapshot>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, server_id, taken_at, digest, label, snapshot_json FROM snapshots
             WHERE server_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![server_id, limit as i64], snapshot_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(build_snapshot).collect()
    }

    /// Snapshots that carry a name, newest first. Deliberately unlimited: a
    /// baseline pinned long ago must stay reachable after the timeline's
    /// window has moved past it.
    pub fn labeled_snapshots(&self, server_id: ServerId) -> Result<Vec<StoredSnapshot>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, server_id, taken_at, digest, label, snapshot_json FROM snapshots
             WHERE server_id = ?1 AND label IS NOT NULL ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map(params![server_id], snapshot_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(build_snapshot).collect()
    }

    /// Pin a name on a snapshot, or clear it with `None`.
    pub fn set_snapshot_label(&self, id: SnapshotId, label: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE snapshots SET label = ?1 WHERE id = ?2",
            params![label, id],
        )?;
        Ok(())
    }

    // ── Call history ────────────────────────────────────────────────────────

    pub fn record_call(&self, call: NewCall) -> Result<CallId> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO calls
             (server_id, kind, target, request_json, response_json, is_error, duration_ms, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                call.server_id,
                call.kind.as_str(),
                call.target,
                call.request.to_string(),
                call.response.to_string(),
                call.is_error,
                call.duration_ms,
                now(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Call history, newest first. `server_id` of `None` spans every server.
    pub fn calls(&self, server_id: Option<ServerId>, limit: usize) -> Result<Vec<CallRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, server_id, kind, target, request_json, response_json,
                    is_error, duration_ms, started_at
             FROM calls
             WHERE (?1 IS NULL OR server_id = ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![server_id, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, bool>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        rows.into_iter()
            .map(
                |(id, server_id, kind, target, request, response, is_error, duration_ms, at)| {
                    Ok(CallRow {
                        id,
                        server_id,
                        kind: CallKind::parse(&kind),
                        target,
                        request: serde_json::from_str(&request)?,
                        response: serde_json::from_str(&response)?,
                        is_error,
                        duration_ms,
                        started_at: parse_time(&at),
                    })
                },
            )
            .collect()
    }

    /// For every tool on `server_id`, its most recent call — but only where
    /// that call failed. Yields `(target, response text, when)`.
    ///
    /// Only the latest call counts, so a tool that failed once and has since
    /// succeeded drops out on its own and the caller never has to expire a
    /// mark it set. What the failure *meant* is deliberately not decided here:
    /// that reading is a heuristic over the server's own prose, and the store
    /// does not do heuristics.
    pub fn last_call_failures(
        &self,
        server_id: ServerId,
    ) -> Result<Vec<(String, String, DateTime<Utc>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.target, c.response_json, c.started_at
             FROM calls c
             JOIN (SELECT target, MAX(id) AS id
                   FROM calls
                   WHERE server_id = ?1 AND kind = 'tool'
                   GROUP BY target) latest ON latest.id = c.id
             WHERE c.is_error = 1",
        )?;
        let rows = stmt
            .query_map(params![server_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(target, response, at)| (target, response, parse_time(&at)))
            .collect())
    }

    // ── Settings ────────────────────────────────────────────────────────────

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

type SnapshotTuple = (i64, i64, String, String, Option<String>, String);

fn snapshot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn build_snapshot(
    (id, server_id, taken_at, digest, label, json): SnapshotTuple,
) -> Result<StoredSnapshot> {
    Ok(StoredSnapshot {
        id,
        server_id,
        taken_at: parse_time(&taken_at),
        digest,
        label,
        snapshot: serde_json::from_str(&json)?,
    })
}

/// Timestamps are stored as RFC 3339 text.
///
/// SQLite has no date type, and text keeps rows readable when debugging the
/// database by hand — which for a local store is the usual way in.
fn now() -> String {
    Utc::now().to_rfc3339()
}

fn parse_time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::UNIX_EPOCH)
}

pub type CollectionId = i64;
pub type StepId = i64;

/// A saved sequence of calls — a smoke test for one server.
#[derive(Debug, Clone, PartialEq)]
pub struct Collection {
    pub id: CollectionId,
    pub server_id: ServerId,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// One call in a collection. Mirrors [`NewCall`] minus the outcome, because a
/// step is exactly "a call you already know you want to make again".
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub id: StepId,
    pub collection_id: CollectionId,
    pub kind: CallKind,
    pub target: String,
    pub request: serde_json::Value,
}

impl Store {
    pub fn create_collection(&self, server_id: ServerId, name: &str) -> Result<CollectionId> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO collections (server_id, name, created_at) VALUES (?1, ?2, ?3)",
            params![server_id, name, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn collections(&self, server_id: ServerId) -> Result<Vec<Collection>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, server_id, name, created_at FROM collections
             WHERE server_id = ?1 ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map(params![server_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(rows
            .into_iter()
            .map(|(id, server_id, name, created_at)| Collection {
                id,
                server_id,
                name,
                created_at: parse_time(&created_at),
            })
            .collect())
    }

    pub fn rename_collection(&self, id: CollectionId, name: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE collections SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    /// Removes the collection and, by cascade, its steps.
    pub fn delete_collection(&self, id: CollectionId) -> Result<()> {
        self.conn()
            .execute("DELETE FROM collections WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Append a step to the end of a collection.
    pub fn add_step(
        &self,
        collection_id: CollectionId,
        kind: CallKind,
        target: &str,
        request: &serde_json::Value,
    ) -> Result<StepId> {
        let conn = self.conn();
        // Ordering is sparse, so appending only needs the current maximum —
        // no renumbering, and concurrent writers are not a concern in a
        // single-user desktop app.
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(ord), 0) + 1 FROM collection_steps WHERE collection_id = ?1",
            params![collection_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO collection_steps (collection_id, ord, kind, target, request_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                collection_id,
                next,
                kind.as_str(),
                target,
                request.to_string()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn steps(&self, collection_id: CollectionId) -> Result<Vec<Step>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, collection_id, kind, target, request_json FROM collection_steps
             WHERE collection_id = ?1 ORDER BY ord",
        )?;
        let rows = stmt
            .query_map(params![collection_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        rows.into_iter()
            .map(|(id, collection_id, kind, target, request)| {
                Ok(Step {
                    id,
                    collection_id,
                    kind: CallKind::parse(&kind),
                    target,
                    request: serde_json::from_str(&request)?,
                })
            })
            .collect()
    }

    pub fn delete_step(&self, id: StepId) -> Result<()> {
        self.conn()
            .execute("DELETE FROM collection_steps WHERE id = ?1", params![id])?;
        Ok(())
    }
}
