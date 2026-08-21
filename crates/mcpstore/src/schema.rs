//! Schema creation and migration.
//!
//! Versioned with `PRAGMA user_version` rather than a migration crate: this is
//! a single-user local database with no replicas and no checksum bookkeeping to
//! honour, so a list of steps applied in order is the whole requirement.
//!
//! To change the schema, append a step. Never edit an existing one — a database
//! already past that version will not replay it.

use rusqlite::Connection;

/// Each entry is one version step, applied in order.
const STEPS: &[&str] = &[
    // v1 — servers, snapshots, call history, settings.
    r#"
    CREATE TABLE servers (
        id                INTEGER PRIMARY KEY,
        name              TEXT NOT NULL,
        transport_kind    TEXT NOT NULL,
        config_json       TEXT NOT NULL,
        created_at        TEXT NOT NULL,
        last_connected_at TEXT
    );

    CREATE TABLE snapshots (
        id            INTEGER PRIMARY KEY,
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        taken_at      TEXT NOT NULL,
        -- blake3 of the snapshot's canonical form; a new row is only written
        -- when this differs from the server's previous snapshot.
        digest        TEXT NOT NULL,
        snapshot_json TEXT NOT NULL
    );
    CREATE INDEX snapshots_by_server ON snapshots(server_id, id DESC);

    CREATE TABLE calls (
        id            INTEGER PRIMARY KEY,
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        kind          TEXT NOT NULL,
        target        TEXT NOT NULL,
        request_json  TEXT NOT NULL,
        response_json TEXT NOT NULL,
        is_error      INTEGER NOT NULL,
        duration_ms   INTEGER NOT NULL,
        started_at    TEXT NOT NULL
    );
    CREATE INDEX calls_by_server ON calls(server_id, id DESC);

    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // v2 — saved call sequences.
    r#"
    CREATE TABLE collections (
        id         INTEGER PRIMARY KEY,
        server_id  INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        name       TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX collections_by_server ON collections(server_id, name);

    CREATE TABLE collection_steps (
        id            INTEGER PRIMARY KEY,
        collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
        -- Sparse on purpose: deleting a step leaves a gap rather than
        -- renumbering, and ordering only ever compares.
        ord           INTEGER NOT NULL,
        kind          TEXT NOT NULL,
        target        TEXT NOT NULL,
        request_json  TEXT NOT NULL
    );
    CREATE INDEX collection_steps_in_order ON collection_steps(collection_id, ord);
    "#,
    // v3 — named baselines: a snapshot can carry a human name ("v1.2").
    // A column rather than a table because a baseline *is* a labelled
    // snapshot; the partial index keeps a name unambiguous per server.
    r#"
    ALTER TABLE snapshots ADD COLUMN label TEXT;
    CREATE UNIQUE INDEX snapshots_label_per_server
        ON snapshots(server_id, label) WHERE label IS NOT NULL;
    "#,
];

pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let current = current as usize;

    for (index, step) in STEPS.iter().enumerate().skip(current) {
        conn.execute_batch(step)?;
        // `pragma_update` will not interpolate, so the version is formatted in.
        // It is derived from a constant array index, not from user input.
        conn.pragma_update(None, "user_version", (index + 1) as i64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_twice_is_a_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version as usize, STEPS.len());
    }

    #[test]
    fn a_fresh_database_lands_on_the_latest_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            tables,
            [
                "calls",
                "collection_steps",
                "collections",
                "servers",
                "settings",
                "snapshots"
            ]
        );
    }
}
