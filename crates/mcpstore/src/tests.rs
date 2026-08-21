//! Every test runs against an in-memory database, so the suite needs no
//! fixtures on disk and no cleanup.

use std::collections::BTreeMap;

use schemadiff::{Severity, Snapshot};
use serde_json::json;

use super::*;

fn store() -> Store {
    Store::open_in_memory().expect("in-memory database opens")
}

fn stdio_server(name: &str) -> NewServer {
    NewServer {
        name: name.into(),
        transport_kind: TransportKind::Stdio,
        config: json!({ "command": "mockserver", "args": ["--variant", "a"] }),
    }
}

/// A snapshot with one tool, whose `query` property is optional or required.
fn snapshot_with(required: bool) -> Snapshot {
    let tool = json!({
        "name": "search",
        "inputSchema": {
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": if required { json!(["query"]) } else { json!([]) },
        },
    });
    Snapshot {
        protocol_version: "2025-06-18".into(),
        server_name: "fixture".into(),
        server_version: "1.0.0".into(),
        capabilities: json!({ "tools": {} }),
        tools: BTreeMap::from([("search".to_string(), tool)]),
        ..Default::default()
    }
}

// ── Servers ─────────────────────────────────────────────────────────────────

#[test]
fn servers_round_trip() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();

    let row = store.get_server(id).unwrap();
    assert_eq!(row.name, "Local");
    assert_eq!(row.transport_kind, TransportKind::Stdio);
    assert_eq!(row.config["command"], "mockserver");
    assert!(row.last_connected_at.is_none());
}

#[test]
fn servers_are_listed_case_insensitively_by_name() {
    let store = store();
    for name in ["zebra", "Apple", "mango"] {
        store.add_server(stdio_server(name)).unwrap();
    }
    let names: Vec<String> = store
        .list_servers()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["Apple", "mango", "zebra"]);
}

#[test]
fn updating_and_deleting_an_unknown_server_is_an_error() {
    let store = store();
    assert!(matches!(
        store.update_server(404, "x", &json!({})),
        Err(Error::UnknownServer(404))
    ));
    assert!(matches!(
        store.delete_server(404),
        Err(Error::UnknownServer(404))
    ));
    assert!(matches!(
        store.get_server(404),
        Err(Error::UnknownServer(404))
    ));
}

#[test]
fn marking_connected_records_a_timestamp() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    store.mark_connected(id).unwrap();
    assert!(store.get_server(id).unwrap().last_connected_at.is_some());
}

#[test]
fn deleting_a_server_takes_its_snapshots_and_calls_with_it() {
    // Foreign keys are off by default in SQLite; this is the test that catches
    // the pragma going missing.
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    store.record_snapshot(id, &snapshot_with(false)).unwrap();
    store
        .record_call(NewCall {
            server_id: id,
            kind: CallKind::Tool,
            target: "search".into(),
            request: json!({}),
            response: json!({}),
            is_error: false,
            duration_ms: 1,
        })
        .unwrap();

    store.delete_server(id).unwrap();

    assert!(store.latest_snapshot(id).unwrap().is_none());
    assert!(store.calls(Some(id), 10).unwrap().is_empty());
}

// ── Snapshots ───────────────────────────────────────────────────────────────

#[test]
fn the_first_snapshot_has_nothing_to_compare_against() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();

    let outcome = store.record_snapshot(id, &snapshot_with(false)).unwrap();
    assert!(matches!(outcome, SnapshotOutcome::First { .. }));
    assert!(outcome.diff().is_none());
}

#[test]
fn an_identical_snapshot_writes_no_new_row() {
    // The whole point of the digest: reconnecting all day must not fill the
    // table with duplicates.
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let snapshot = snapshot_with(false);

    let first = store.record_snapshot(id, &snapshot).unwrap();
    let second = store.record_snapshot(id, &snapshot).unwrap();
    let third = store.record_snapshot(id, &snapshot).unwrap();

    assert!(matches!(second, SnapshotOutcome::Unchanged { .. }));
    assert!(matches!(third, SnapshotOutcome::Unchanged { .. }));
    assert_eq!(second.id(), first.id());
    assert_eq!(store.snapshots(id, 10).unwrap().len(), 1);
}

#[test]
fn key_order_alone_does_not_count_as_a_change() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();

    let mut reordered = snapshot_with(false);
    let tool = reordered.tools.get_mut("search").unwrap();
    // Re-parse with the keys in a different textual order.
    *tool = serde_json::from_str(
        r#"{"inputSchema":{"required":[],"properties":{"query":{"type":"string"}},"type":"object"},"name":"search"}"#,
    )
    .unwrap();

    store.record_snapshot(id, &snapshot_with(false)).unwrap();
    let outcome = store.record_snapshot(id, &reordered).unwrap();

    assert!(
        matches!(outcome, SnapshotOutcome::Unchanged { .. }),
        "reordered keys must not register as a change, got {outcome:?}"
    );
}

#[test]
fn a_changed_snapshot_comes_back_with_a_classified_diff() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();

    let first = store.record_snapshot(id, &snapshot_with(false)).unwrap();
    let outcome = store.record_snapshot(id, &snapshot_with(true)).unwrap();

    match &outcome {
        SnapshotOutcome::Changed { previous, diff, .. } => {
            assert_eq!(*previous, first.id());
            assert_eq!(diff.severity(), Some(Severity::Breaking));
            assert_eq!(diff.breaking_items(), vec!["search"]);
        }
        other => panic!("expected Changed, got {other:?}"),
    }
    assert_eq!(store.snapshots(id, 10).unwrap().len(), 2);
}

#[test]
fn snapshots_come_back_newest_first_and_deserialize_intact() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    store.record_snapshot(id, &snapshot_with(false)).unwrap();
    store.record_snapshot(id, &snapshot_with(true)).unwrap();

    let history = store.snapshots(id, 10).unwrap();
    assert_eq!(history.len(), 2);
    assert!(history[0].id > history[1].id);
    // The round trip through JSON must preserve the schema exactly, or diffs
    // against stored history would be nonsense.
    assert_eq!(history[0].snapshot, snapshot_with(true));
    assert_eq!(history[1].snapshot, snapshot_with(false));
}

#[test]
fn snapshots_are_scoped_per_server() {
    let store = store();
    let a = store.add_server(stdio_server("A")).unwrap();
    let b = store.add_server(stdio_server("B")).unwrap();

    store.record_snapshot(a, &snapshot_with(false)).unwrap();
    let outcome = store.record_snapshot(b, &snapshot_with(true)).unwrap();

    // B has its own history, so its first snapshot is a First, not a diff
    // against A's.
    assert!(matches!(outcome, SnapshotOutcome::First { .. }));
}

// ── Named baselines ─────────────────────────────────────────────────────────

#[test]
fn a_snapshot_can_be_pinned_under_a_name_and_unpinned() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let first = store.record_snapshot(id, &snapshot_with(false)).unwrap();

    store.set_snapshot_label(first.id(), Some("v1.2")).unwrap();
    assert_eq!(
        store.snapshots(id, 10).unwrap()[0].label.as_deref(),
        Some("v1.2")
    );
    assert_eq!(store.labeled_snapshots(id).unwrap().len(), 1);

    store.set_snapshot_label(first.id(), None).unwrap();
    assert!(store.labeled_snapshots(id).unwrap().is_empty());
}

#[test]
fn a_name_pins_at_most_one_snapshot_per_server() {
    let store = store();
    let a = store.add_server(stdio_server("A")).unwrap();
    let b = store.add_server(stdio_server("B")).unwrap();
    let a_first = store.record_snapshot(a, &snapshot_with(false)).unwrap();
    let a_second = store.record_snapshot(a, &snapshot_with(true)).unwrap();
    let b_first = store.record_snapshot(b, &snapshot_with(false)).unwrap();

    store.set_snapshot_label(a_first.id(), Some("v1")).unwrap();
    assert!(
        store.set_snapshot_label(a_second.id(), Some("v1")).is_err(),
        "the same name on a second snapshot of the same server must refuse"
    );
    // A different server is a different namespace.
    store.set_snapshot_label(b_first.id(), Some("v1")).unwrap();
}

#[test]
fn a_baseline_outlives_the_timeline_window() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let pinned = store.record_snapshot(id, &snapshot_with(false)).unwrap();
    store.record_snapshot(id, &snapshot_with(true)).unwrap();
    store.set_snapshot_label(pinned.id(), Some("v1")).unwrap();

    // A window of one no longer includes the pinned row…
    let window = store.snapshots(id, 1).unwrap();
    assert!(window.iter().all(|s| s.id != pinned.id()));
    // …but the baseline is still reachable.
    assert_eq!(
        store.labeled_snapshots(id).unwrap()[0].label.as_deref(),
        Some("v1")
    );
}

// ── Call history ────────────────────────────────────────────────────────────

#[test]
fn calls_round_trip_and_come_back_newest_first() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();

    for i in 0..3 {
        store
            .record_call(NewCall {
                server_id: id,
                kind: CallKind::Tool,
                target: format!("tool{i}"),
                request: json!({ "n": i }),
                response: json!({ "ok": true }),
                is_error: false,
                duration_ms: 10 + i,
            })
            .unwrap();
    }

    let calls = store.calls(Some(id), 10).unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].target, "tool2");
    assert_eq!(calls[0].request["n"], 2);
    assert_eq!(calls[0].duration_ms, 12);
    assert!(!calls[0].is_error);
}

#[test]
fn calls_can_be_read_across_every_server_or_filtered_to_one() {
    let store = store();
    let a = store.add_server(stdio_server("A")).unwrap();
    let b = store.add_server(stdio_server("B")).unwrap();

    for server_id in [a, b] {
        store
            .record_call(NewCall {
                server_id,
                kind: CallKind::Resource,
                target: "mock://notes".into(),
                request: json!({}),
                response: json!({}),
                is_error: true,
                duration_ms: 5,
            })
            .unwrap();
    }

    assert_eq!(store.calls(None, 10).unwrap().len(), 2);
    assert_eq!(store.calls(Some(a), 10).unwrap().len(), 1);
    assert_eq!(
        store.calls(Some(a), 10).unwrap()[0].kind,
        CallKind::Resource
    );
    assert!(store.calls(Some(a), 10).unwrap()[0].is_error);
}

#[test]
fn the_call_limit_is_honoured() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    for i in 0..5 {
        store
            .record_call(NewCall {
                server_id: id,
                kind: CallKind::Tool,
                target: format!("t{i}"),
                request: json!({}),
                response: json!({}),
                is_error: false,
                duration_ms: 1,
            })
            .unwrap();
    }
    assert_eq!(store.calls(Some(id), 2).unwrap().len(), 2);
}

// ── Settings ────────────────────────────────────────────────────────────────

#[test]
fn settings_upsert() {
    let store = store();
    assert_eq!(store.setting("resolved_path").unwrap(), None);

    store.set_setting("resolved_path", "/usr/bin:/bin").unwrap();
    assert_eq!(
        store.setting("resolved_path").unwrap().as_deref(),
        Some("/usr/bin:/bin")
    );

    store
        .set_setting("resolved_path", "/opt/homebrew/bin")
        .unwrap();
    assert_eq!(
        store.setting("resolved_path").unwrap().as_deref(),
        Some("/opt/homebrew/bin")
    );
}

// ── Collections ─────────────────────────────────────────────────────────────

#[test]
fn collections_round_trip_and_hold_their_steps_in_order() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let collection = store.create_collection(id, "Smoke test").unwrap();

    for target in ["first", "second", "third"] {
        store
            .add_step(
                collection,
                CallKind::Tool,
                target,
                &json!({ "arg": target }),
            )
            .unwrap();
    }

    let collections = store.collections(id).unwrap();
    assert_eq!(collections.len(), 1);
    assert_eq!(collections[0].name, "Smoke test");

    let steps = store.steps(collection).unwrap();
    let targets: Vec<&str> = steps.iter().map(|s| s.target.as_str()).collect();
    assert_eq!(targets, ["first", "second", "third"]);
    assert_eq!(steps[1].request["arg"], "second");
    assert_eq!(steps[0].kind, CallKind::Tool);
}

#[test]
fn deleting_a_middle_step_leaves_the_rest_in_order() {
    // Ordering is sparse, so a gap must not disturb what remains.
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let collection = store.create_collection(id, "Smoke").unwrap();

    let mut ids = Vec::new();
    for target in ["a", "b", "c"] {
        ids.push(
            store
                .add_step(collection, CallKind::Tool, target, &json!({}))
                .unwrap(),
        );
    }
    store.delete_step(ids[1]).unwrap();

    let targets: Vec<String> = store
        .steps(collection)
        .unwrap()
        .into_iter()
        .map(|s| s.target)
        .collect();
    assert_eq!(targets, ["a", "c"]);

    // And a later append still lands at the end rather than reusing the gap.
    store
        .add_step(collection, CallKind::Tool, "d", &json!({}))
        .unwrap();
    let targets: Vec<String> = store
        .steps(collection)
        .unwrap()
        .into_iter()
        .map(|s| s.target)
        .collect();
    assert_eq!(targets, ["a", "c", "d"]);
}

#[test]
fn collections_are_scoped_per_server() {
    let store = store();
    let a = store.add_server(stdio_server("A")).unwrap();
    let b = store.add_server(stdio_server("B")).unwrap();
    store.create_collection(a, "For A").unwrap();

    assert_eq!(store.collections(a).unwrap().len(), 1);
    assert!(store.collections(b).unwrap().is_empty());
}

#[test]
fn deleting_a_collection_takes_its_steps_with_it() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let collection = store.create_collection(id, "Smoke").unwrap();
    store
        .add_step(collection, CallKind::Tool, "echo", &json!({}))
        .unwrap();

    store.delete_collection(collection).unwrap();
    assert!(store.steps(collection).unwrap().is_empty());
}

#[test]
fn deleting_a_server_takes_its_collections_with_it() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let collection = store.create_collection(id, "Smoke").unwrap();
    store
        .add_step(collection, CallKind::Tool, "echo", &json!({}))
        .unwrap();

    store.delete_server(id).unwrap();

    assert!(store.collections(id).unwrap().is_empty());
    assert!(
        store.steps(collection).unwrap().is_empty(),
        "the cascade has to reach two levels down"
    );
}

#[test]
fn a_collection_can_be_renamed() {
    let store = store();
    let id = store.add_server(stdio_server("Local")).unwrap();
    let collection = store.create_collection(id, "Old").unwrap();
    store.rename_collection(collection, "New").unwrap();
    assert_eq!(store.collections(id).unwrap()[0].name, "New");
}
