//! Exercises the pipeline `AppState::connect` runs, without a Dioxus runtime.
//!
//! `connect` does five things in order: read the row, turn its stored config
//! into a transport, dial it, snapshot the contract, and record that snapshot.
//! Everything except the final signal writes is plain logic, and this is where
//! it gets checked — a screenshot proves the window opened, not that connecting
//! to a real server produces the right rows.

use std::path::PathBuf;

use mcpclient::Handle;
use mcpstore::{NewServer, SnapshotOutcome, Store, TransportKind};
use schemadiff::Severity;

use crate::config;

/// Locate the fixture binary next to this test executable.
fn mockserver_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("test executable has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let binary = dir.join("mockserver");
    assert!(
        binary.exists(),
        "fixture not built at {}. Run `cargo build -p mockserver` first.",
        binary.display()
    );
    binary
}

/// The JSON a saved stdio server row holds, as the dialog would have written it.
fn stored_config(variant: &str) -> serde_json::Value {
    serde_json::json!({
        "command": mockserver_binary().to_string_lossy(),
        "args": ["--variant", variant],
        "env": {},
    })
}

#[tokio::test]
async fn a_saved_row_connects_snapshots_and_records() {
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();

    let row = store.get_server(id).unwrap();
    let transport = config::to_transport(row.transport_kind, &row.config, row.id)
        .expect("a row written by the dialog must dial");

    let (handle, info) = Handle::connect(&transport).await.expect("connect");
    assert!(info.server_info.is_some());

    let snapshot = handle.snapshot().await.expect("snapshot");
    assert!(snapshot.tools.contains_key("search"));
    assert!(snapshot.resources.contains_key("mock://notes"));
    assert!(snapshot.prompts.contains_key("review"));

    let outcome = store.record_snapshot(id, &snapshot).unwrap();
    assert!(matches!(outcome, SnapshotOutcome::First { .. }));

    store.mark_connected(id).unwrap();
    assert!(store.get_server(id).unwrap().last_connected_at.is_some());

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnecting_after_the_server_changed_yields_a_breaking_diff() {
    // The scenario the product exists for: same saved server, edited to point at
    // a build whose contract moved. What the sidebar badges and the browser's
    // per-row markers read comes straight off this outcome.
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();

    for variant in ["a", "b"] {
        store
            .update_server(id, "Mock", &stored_config(variant))
            .unwrap();
        let row = store.get_server(id).unwrap();
        let transport = config::to_transport(row.transport_kind, &row.config, row.id).unwrap();
        let (handle, _) = Handle::connect(&transport).await.expect("connect");
        let snapshot = handle.snapshot().await.expect("snapshot");
        store.record_snapshot(id, &snapshot).unwrap();
        handle.shutdown().await.unwrap();
    }

    let history = store.snapshots(id, 10).unwrap();
    assert_eq!(history.len(), 2, "both contracts should have been recorded");

    let diff = schemadiff::diff(&history[1].snapshot, &history[0].snapshot);
    assert_eq!(diff.severity(), Some(Severity::Breaking));
    assert!(diff.breaking_items().contains(&"search"));
    assert!(diff.counts().breaking >= 2);
}

#[tokio::test]
async fn a_row_with_an_unreadable_command_fails_with_an_actionable_message() {
    // This is the message that lands in the middle pane's "Could not connect"
    // panel, so it has to name the binary rather than say "No such file".
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Broken".into(),
            transport_kind: TransportKind::Stdio,
            config: serde_json::json!({
                "command": "definitely-not-a-real-mcp-server",
                "env": { "PATH": "/usr/bin:/bin" },
            }),
        })
        .unwrap();

    let row = store.get_server(id).unwrap();
    let transport = config::to_transport(row.transport_kind, &row.config, row.id).unwrap();
    let error = Handle::connect(&transport)
        .await
        .expect_err("cannot connect");

    let message = error.to_string();
    assert!(
        message.contains("definitely-not-a-real-mcp-server") && message.contains("/usr/bin"),
        "the panel needs a message naming the binary and where it looked: {message}"
    );
}

// ── The call path (Phase 5) ─────────────────────────────────────────────────

use crate::form;
use crate::state::{Selection, Tab, execute};
use mcpstore::{CallKind, NewCall};
use serde_json::json;

/// Connect to the fixture and hand back a live handle.
async fn connect(variant: &str) -> mcpclient::Handle {
    let transport = config::to_transport(TransportKind::Stdio, &stored_config(variant), 1).unwrap();
    let (handle, _) = Handle::connect(&transport).await.expect("connect");
    handle
}

#[tokio::test]
async fn a_real_tool_schema_seeds_the_form_with_its_required_fields() {
    let handle = connect("b").await;
    let snapshot = handle.snapshot().await.unwrap();

    let schema = snapshot.tools["search"]["inputSchema"].clone();
    let seeded = form::seed(&schema);

    // Variant b requires both; `limit`, `cursor` and `mode` are optional and so
    // must stay out of the payload entirely.
    assert_eq!(seeded["query"], json!(""));
    assert_eq!(seeded["index"], json!(""));
    assert!(seeded.get("cursor").is_none());
    assert!(seeded.get("limit").is_none());

    // `mode` is a closed set, so the form must offer a select rather than a
    // free-text box.
    let properties = form::properties_of(&schema);
    let mode = properties.iter().find(|p| p.name == "mode").unwrap();
    assert!(matches!(mode.widget, form::Widget::Choice(_)));

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn calling_a_tool_records_it_and_replaying_reproduces_the_result() {
    // This is the Phase 5 gate: run a call, find it in history, run it again
    // from the stored request, and get the same thing back.
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();
    let handle = connect("a").await;

    let selection = Selection {
        tab: Tab::Tools,
        name: "echo".into(),
    };
    let request = json!({ "message": "round trip" });

    let response = execute(&handle, &selection, &request).await.expect("call");
    assert!(
        serde_json::to_string(&response)
            .unwrap()
            .contains("round trip"),
        "the fixture echoes its argument back: {response}"
    );

    store
        .record_call(NewCall {
            server_id: id,
            kind: CallKind::Tool,
            target: selection.name.clone(),
            request: request.clone(),
            response: response.clone(),
            is_error: false,
            duration_ms: 1,
        })
        .unwrap();

    // What the history row hands back to the form on click.
    let history = store.calls(Some(id), 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].target, "echo");
    assert_eq!(history[0].request, request);

    let replayed = execute(&handle, &selection, &history[0].request)
        .await
        .expect("replay");
    assert_eq!(
        replayed, response,
        "replaying a stored request must reproduce the original response"
    );

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tool_error_is_reported_without_losing_the_response() {
    // A tool that fails is not a transport failure: the pane still has a
    // response to render, and history still gets a row.
    let handle = connect("a").await;
    let selection = Selection {
        tab: Tab::Tools,
        name: "echo".into(),
    };

    // `echo` requires `message`; omitting it makes the fixture reject the call.
    let result = execute(&handle, &selection, &json!({})).await;
    match result {
        Err(e) => assert!(!e.to_string().is_empty(), "an error must say something"),
        Ok(response) => assert_eq!(
            response.get("isError").and_then(serde_json::Value::as_bool),
            Some(true),
            "a rejected call must be flagged: {response}"
        ),
    }

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn resources_and_prompts_go_through_the_same_dispatch() {
    let handle = connect("a").await;

    let resource = execute(
        &handle,
        &Selection {
            tab: Tab::Resources,
            name: "mock://notes".into(),
        },
        &json!({}),
    )
    .await
    .expect("resource read");
    assert!(
        serde_json::to_string(&resource)
            .unwrap()
            .contains("Fixture body"),
        "{resource}"
    );

    let prompt = execute(
        &handle,
        &Selection {
            tab: Tab::Prompts,
            name: "review".into(),
        },
        &json!({ "style": "terse" }),
    )
    .await
    .expect("prompt get");
    assert!(prompt.get("messages").is_some(), "{prompt}");

    handle.shutdown().await.unwrap();
}

// ── The diff surface (Phase 6) ──────────────────────────────────────────────

use crate::state::{Connected, ContractStatus};

/// Connect, snapshot, record, and build the `Connected` the panes read.
async fn observe(store: &Store, id: mcpstore::ServerId, variant: &str) -> Connected {
    store
        .update_server(id, "Mock", &stored_config(variant))
        .unwrap();
    let row = store.get_server(id).unwrap();
    let transport = config::to_transport(row.transport_kind, &row.config, row.id).unwrap();
    let (handle, info) = Handle::connect(&transport).await.expect("connect");
    let snapshot = handle.snapshot().await.expect("snapshot");

    let status = match store.record_snapshot(id, &snapshot).unwrap() {
        mcpstore::SnapshotOutcome::Changed { diff, .. } => ContractStatus::Changed(diff),
        mcpstore::SnapshotOutcome::First { .. } => ContractStatus::First,
        mcpstore::SnapshotOutcome::Unchanged { .. } => ContractStatus::Unchanged,
    };

    Connected {
        handle,
        snapshot,
        status,
        instructions: info.instructions.clone(),
    }
}

#[tokio::test]
async fn the_contract_header_distinguishes_first_from_unchanged_from_changed() {
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();

    let first = observe(&store, id, "a").await;
    assert!(matches!(first.status, ContractStatus::First));
    first.handle.shutdown().await.unwrap();

    // Same server, same contract: the banner must say so rather than implying
    // this is a fresh acquaintance.
    let again = observe(&store, id, "a").await;
    assert!(matches!(again.status, ContractStatus::Unchanged));
    again.handle.shutdown().await.unwrap();

    let moved = observe(&store, id, "b").await;
    assert!(matches!(moved.status, ContractStatus::Changed(_)));
    moved.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_changed_contract_drives_every_badge_the_ui_shows() {
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();

    observe(&store, id, "a")
        .await
        .handle
        .shutdown()
        .await
        .unwrap();
    let connected = observe(&store, id, "b").await;

    let diff = connected
        .status
        .diff()
        .expect("variant b differs from variant a");
    let counts = diff.counts();

    // The sidebar badge and the banner both read this number.
    assert!(
        counts.breaking >= 3,
        "expected several breaks, got {counts:?}"
    );
    assert!(counts.total() > counts.breaking, "some changes are benign");

    // The inline panel in the detail pane.
    let changed = connected.change_for(&Selection {
        tab: Tab::Tools,
        name: "search".into(),
    });
    let changed = changed.expect("search moved between variants");
    assert_eq!(changed.severity(), Severity::Breaking);

    // A tool identical in both variants must show nothing at all — a diff tool
    // that cries wolf on unchanged items is worse than none.
    assert!(
        connected
            .change_for(&Selection {
                tab: Tab::Tools,
                name: "echo".into(),
            })
            .is_none(),
        "echo is byte-identical in both variants"
    );

    // The removed tool is gone from the snapshot but present in the diff, which
    // is what lets the drawer list it at all.
    assert!(!connected.snapshot.tools.contains_key("deprecated_tool"));
    assert!(
        diff.tools.iter().any(|c| c.name() == "deprecated_tool"),
        "a removed tool has to survive into the diff or it vanishes silently"
    );

    // Field-level notes are what the drawer prints under each path.
    let fields = match &changed {
        schemadiff::ItemChange::Modified { fields, .. } => fields.clone(),
        other => panic!("expected Modified, got {other:?}"),
    };
    assert!(
        fields
            .iter()
            .any(|f| f.path.ends_with("properties.index") && f.severity == Severity::Breaking),
        "the added required property must be flagged: {:?}",
        fields.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
    assert!(
        fields.iter().any(|f| f.severity == Severity::Cosmetic),
        "the reworded description must stay cosmetic"
    );

    connected.handle.shutdown().await.unwrap();
}

// ── Collections (Phase 8) ───────────────────────────────────────────────────

use mcpstore::CollectionId;

/// Run every step of a collection in order, mirroring `AppState::run_collection`
/// without needing a Dioxus runtime. Returns `(target, failed)` per step.
async fn run_steps(
    store: &Store,
    collection: CollectionId,
    handle: &mcpclient::Handle,
) -> Vec<(String, bool)> {
    let mut outcomes = Vec::new();
    for step in store.steps(collection).unwrap() {
        let selection = Selection {
            tab: match step.kind {
                CallKind::Tool => Tab::Tools,
                CallKind::Resource => Tab::Resources,
                CallKind::Prompt => Tab::Prompts,
            },
            name: step.target.clone(),
        };
        let failed = match execute(handle, &selection, &step.request).await {
            Ok(response) => response
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            Err(_) => true,
        };
        outcomes.push((step.target, failed));
    }
    outcomes
}

#[tokio::test]
async fn a_smoke_test_passes_against_one_build_and_fails_against_the_next() {
    // The scenario collections exist for: three calls you make every time, run
    // against a server whose contract moved out from under them.
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();

    let collection = store.create_collection(id, "Smoke test").unwrap();
    store
        .add_step(
            collection,
            CallKind::Tool,
            "echo",
            &json!({ "message": "ping" }),
        )
        .unwrap();
    store
        .add_step(
            collection,
            CallKind::Tool,
            "search",
            &json!({ "query": "anything" }),
        )
        .unwrap();
    // Present in variant a, gone in variant b.
    store
        .add_step(collection, CallKind::Tool, "deprecated_tool", &json!({}))
        .unwrap();

    let handle = connect("a").await;
    let against_a = run_steps(&store, collection, &handle).await;
    handle.shutdown().await.unwrap();

    assert_eq!(against_a.len(), 3, "every step should have run");
    assert!(
        against_a.iter().all(|(_, failed)| !failed),
        "the collection should be green against the build it was written for: {against_a:?}"
    );

    let handle = connect("b").await;
    let against_b = run_steps(&store, collection, &handle).await;
    handle.shutdown().await.unwrap();

    let failures: Vec<&str> = against_b
        .iter()
        .filter(|(_, failed)| *failed)
        .map(|(target, _)| target.as_str())
        .collect();
    assert_eq!(
        failures,
        ["deprecated_tool"],
        "exactly the removed tool should fail: {against_b:?}"
    );
    // The run must not stop at the first failure — a smoke test's job is to
    // report everything that broke, not just the earliest thing.
    assert_eq!(against_b.len(), 3, "later steps still ran: {against_b:?}");
}

#[tokio::test]
async fn every_step_of_a_run_lands_in_call_history() {
    // Each step is a real call, so it must be openable and replayable on its
    // own afterwards rather than vanishing into a batch.
    let store = Store::open_in_memory().unwrap();
    let id = store
        .add_server(NewServer {
            name: "Mock".into(),
            transport_kind: TransportKind::Stdio,
            config: stored_config("a"),
        })
        .unwrap();
    let collection = store.create_collection(id, "Smoke").unwrap();
    for message in ["one", "two"] {
        store
            .add_step(
                collection,
                CallKind::Tool,
                "echo",
                &json!({ "message": message }),
            )
            .unwrap();
    }

    let handle = connect("a").await;
    for step in store.steps(collection).unwrap() {
        let selection = Selection {
            tab: Tab::Tools,
            name: step.target.clone(),
        };
        let response = execute(&handle, &selection, &step.request).await.unwrap();
        store
            .record_call(mcpstore::NewCall {
                server_id: id,
                kind: step.kind.clone(),
                target: step.target.clone(),
                request: step.request.clone(),
                response,
                is_error: false,
                duration_ms: 1,
            })
            .unwrap();
    }
    handle.shutdown().await.unwrap();

    let history = store.calls(Some(id), 10).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].request["message"], "two", "newest first");
}
