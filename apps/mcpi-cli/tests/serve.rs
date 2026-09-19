//! `mcpi-cli serve` end to end: a temp store with two saved mockserver rows, the CLI spawned
//! over stdio as the gateway, and `mcpclient` as the client on the other side — the same path a
//! coding agent takes, with TypeSafe stood in for by a local endpoint.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::{Json, Router as AxumRouter, routing::post};
use mcpclient::{Handle, Transport};
use mcpstore::{NewServer, Store, TransportKind};
use serde_json::{Value, json};

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

/// Ranks every choice option by token overlap with the request; nouls say "a tool is needed".
async fn fake_systemone(Json(body): Json<Value>) -> Json<Value> {
    let request = body["state"]["request"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();
    let words: Vec<&str> = request
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut answers = serde_json::Map::new();
    for (qid, q) in body["questions"].as_object().unwrap() {
        if q["type"] == "choice" {
            let scores: BTreeMap<String, f64> = q["criteria"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| {
                    let text = format!("{k} {}", v.as_str().unwrap_or("")).to_lowercase();
                    let hits = words.iter().filter(|w| text.contains(*w)).count() as f64;
                    (k.clone(), (3.0 * hits).exp())
                })
                .collect();
            let total: f64 = scores.values().sum();
            let probs: BTreeMap<String, f64> =
                scores.iter().map(|(k, s)| (k.clone(), s / total)).collect();
            let best = probs
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0
                .clone();
            answers.insert(
                qid.clone(),
                json!({"type": "choice", "choice": best, "confidence": 0.9, "probabilities": probs}),
            );
        } else {
            let p = if qid == "prose" { 0.1 } else { 0.9 };
            answers.insert(qid.clone(), json!({"type": "noul", "noul": p}));
        }
    }
    Json(json!({"model": "fake", "answers": answers, "usage": {"input_tokens": 50}}))
}

fn text_of(r: &rmcp::model::CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

#[tokio::test]
async fn serves_the_saved_library_as_a_gateway() {
    let app = AxumRouter::new().route("/v1/systemone", post(fake_systemone));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = std::env::temp_dir().join(format!(
        "mcpi-cli-serve-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store_path = dir.join("store.db");
    let store = Store::open(&store_path).unwrap();
    let mock = mockserver_binary().display().to_string();
    // Two rows on the same fixture, different variants: `search` and `echo` collide across
    // them, `deprecated_tool` and `summarize` are unique to one side each.
    for (name, variant) in [("Alpha Inc", "a"), ("beta", "b")] {
        store
            .add_server(NewServer {
                name: name.into(),
                transport_kind: TransportKind::Stdio,
                config: json!({"command": mock, "args": ["--variant", variant]}),
                group_id: None,
            })
            .unwrap();
    }
    drop(store);

    let transport = Transport::Stdio {
        command: env!("CARGO_BIN_EXE_mcpi-cli").into(),
        args: vec![
            "serve".into(),
            "--store".into(),
            store_path.display().to_string(),
            "--api-key".into(),
            "test".into(),
            "--expose-direct".into(),
            "summarize".into(),
        ],
        env: BTreeMap::from([("TYPESAFE_BASE_URL".to_string(), base)]),
        cwd: None,
    };
    let (gateway, _) = Handle::connect(&transport).await.expect("gateway starts");

    let tools = gateway.list_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    assert_eq!(names, ["call_tool", "find_tools", "summarize"]);

    let found = gateway
        .call_tool(
            "find_tools",
            json!({"request": "search the index for cats, fast mode"})
                .as_object()
                .cloned()
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text_of(&found)).unwrap();
    let top = body["tools"][0]["name"].as_str().unwrap().to_string();
    // Both variants have `search`, so the gateway prefixed them with the (sanitised) row name.
    assert!(
        top == "Alpha_Inc__search" || top == "beta__search",
        "top tool was {top}"
    );
    assert_eq!(body["tools"][0]["inputSchema"]["required"][0], "query");

    let called = gateway
        .call_tool(
            "call_tool",
            json!({"name": top, "arguments": {"query": "cats"}})
                .as_object()
                .cloned()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(called.is_error, Some(true), "{}", text_of(&called));
    assert!(!text_of(&called).is_empty());

    // The directly exposed tool goes straight through, by its gateway name.
    let direct = gateway
        .call_tool(
            "summarize",
            json!({"text": "hello"}).as_object().cloned().unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(direct.is_error, Some(true), "{}", text_of(&direct));

    gateway.shutdown().await.unwrap();

    // Serving also kept the contract history flowing: one snapshot per row.
    let store = Store::open(&store_path).unwrap();
    for row in store.list_servers().unwrap() {
        assert!(
            store.latest_snapshot(row.id).unwrap().is_some(),
            "{}",
            row.name
        );
        assert!(row.last_connected_at.is_some());
    }
    let log = std::fs::read_to_string(dir.join("router.jsonl")).unwrap();
    assert!(
        log.lines()
            .next()
            .unwrap()
            .contains("\"event\":\"catalog\"")
    );
    assert_eq!(log.matches("\"event\":\"call_tool\"").count(), 2);
    let _ = std::fs::remove_dir_all(dir);
}
