//! The gateway against in-process fake upstreams and a fake TypeSafe endpoint: selection,
//! direct exposure, proxying, and the JSONL log's find_tools → call_tool linkage.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{Json, Router as AxumRouter, routing::post};
use mcprouter::gateway::{CallToolArgs, FindToolsArgs};
use mcprouter::log::{JsonlLog, RecordSink};
use mcprouter::{BoxFuture, ContractNote, Entry, Gateway, Jev, Router, RouterSettings, Upstreams};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde_json::{Value, json};

/// Fake `POST /v1/systemone`: scores every choice option by token overlap between the request
/// and the option's description, normalised, so routing is deterministic and offline.
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
        match q["type"].as_str().unwrap() {
            "choice" => {
                let crit = q["criteria"].as_object().unwrap();
                let scores: BTreeMap<String, f64> = crit
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
            }
            "noul" => {
                let p = if qid == "prose" { 0.1 } else { 0.9 };
                answers.insert(qid.clone(), json!({"type": "noul", "noul": p}));
            }
            _ => unreachable!(),
        }
    }
    Json(
        json!({"model": "fake", "answers": answers, "usage": {"input_tokens": 123, "output_tokens": 0}}),
    )
}

async fn fake_typesafe() -> String {
    let app = AxumRouter::new().route("/v1/systemone", post(fake_systemone));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Fake {
    entries: Vec<Entry>,
}

fn entry(server: &str, key: &str, name: &str, desc: &str, field: &str) -> Entry {
    let schema =
        json!({"type": "object", "properties": {field: {"type": "string"}}, "required": [field]});
    Entry {
        key: key.into(),
        server: server.into(),
        tool: Tool::new(
            name.to_string(),
            desc.to_string(),
            Arc::new(schema.as_object().cloned().unwrap()),
        ),
    }
}

impl Upstreams for Fake {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn call<'a>(
        &'a self,
        key: &'a str,
        arguments: Option<JsonObject>,
    ) -> BoxFuture<'a, Result<CallToolResult, String>> {
        Box::pin(async move {
            let e = self
                .entries
                .iter()
                .find(|e| e.key == key)
                .ok_or("unknown")?;
            let args = arguments.map(Value::Object).unwrap_or(Value::Null);
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{}:{}:{}",
                e.server, e.tool.name, args
            ))]))
        })
    }
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

async fn gateway(log: Option<&std::path::Path>) -> Gateway {
    let base = fake_typesafe().await;
    let jev = Jev::new("test", "fake").unwrap().with_base_url(base);
    let entries = vec![
        entry(
            "alpha",
            "alpha_get",
            "alpha_get",
            "Read an alpha record by id",
            "id",
        ),
        entry(
            "alpha",
            "alpha_create",
            "alpha_create",
            "Create a new alpha record",
            "title",
        ),
        entry(
            "alpha",
            "alpha__echo",
            "echo",
            "Echo the arguments back",
            "text",
        ),
        entry(
            "beta",
            "beta_get",
            "beta_get",
            "Read a beta record by id",
            "id",
        ),
        entry(
            "beta",
            "beta_create",
            "beta_create",
            "Create a new beta record",
            "title",
        ),
        entry(
            "beta",
            "beta__echo",
            "echo",
            "Echo the arguments back",
            "text",
        ),
    ];
    let settings = RouterSettings {
        expose_direct: vec!["alpha_get".into(), "missing".into()],
        ..Default::default()
    };
    let router = Router::new(jev, settings, &entries);
    let log: Option<Arc<dyn RecordSink>> = match log {
        Some(p) => Some(Arc::new(JsonlLog::open(p).await.unwrap())),
        None => None,
    };
    let notes = vec![ContractNote {
        server: "beta".into(),
        breaking: 2,
        compatible: 1,
        cosmetic: 0,
    }];
    let g = Gateway::new(Arc::new(Fake { entries }), router, log, notes);
    g.log_catalog().await;
    g
}

#[tokio::test]
async fn routes_proxies_and_logs() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mcprouter-test-{}-{nanos}", std::process::id()));
    let log_path = dir.join("router.jsonl");
    let g = gateway(Some(&log_path)).await;

    // find_tools: the fake scorer prefers lexical overlap, so "read ... alpha ... id" → alpha_get.
    let found = g
        .find_tools_in(
            "t",
            FindToolsArgs {
                request: "read the alpha record with id 42".into(),
                k: None,
            },
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text_of(&found)).unwrap();
    assert_eq!(body["tools"][0]["name"], "alpha_get");
    assert_eq!(body["tools"][0]["server"], "alpha");
    assert_eq!(body["tools"][0]["inputSchema"]["required"][0], "id");
    assert_eq!(body["flat"], false);
    // alpha has no contract note, so none is attached.
    assert!(body.get("contract_changes").is_none());

    // A beta tool carries beta's contract change.
    let found = g
        .find_tools_in(
            "t",
            FindToolsArgs {
                request: "create a new beta record titled hello".into(),
                k: None,
            },
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text_of(&found)).unwrap();
    assert_eq!(body["tools"][0]["name"], "beta_create");
    assert_eq!(body["contract_changes"][0]["breaking"], 2);

    // A vague request is flat: still non-empty, flagged, capped at k.
    let vague = g
        .find_tools_in(
            "t",
            FindToolsArgs {
                request: "hmm".into(),
                k: Some(2),
            },
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text_of(&vague)).unwrap();
    assert_eq!(body["flat"], true);
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);

    // call_tool proxies by gateway key, including prefixed collision names.
    let r = g
        .call_tool_in(
            "t",
            CallToolArgs {
                name: "beta__echo".into(),
                arguments: json!({"text": "hi"}).as_object().cloned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(text_of(&r), r#"beta:echo:{"text":"hi"}"#);
    assert!(
        g.call_tool_in(
            "t",
            CallToolArgs {
                name: "nope".into(),
                arguments: None
            }
        )
        .await
        .is_err()
    );

    // JSONL: catalog snapshot with the contract note, then linked records.
    let log = std::fs::read_to_string(&log_path).unwrap();
    let lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["event"], "catalog");
    assert_eq!(lines[0]["tools"].as_array().unwrap().len(), 6);
    assert_eq!(lines[0]["contract_changes"][0]["server"], "beta");
    assert_eq!(lines[0]["expose_direct"][0], "alpha_get");
    let echo_call = lines.iter().find(|l| l["name"] == "beta__echo").unwrap();
    assert_eq!(echo_call["find_id"], 3);
    assert_eq!(echo_call["in_returned"], false);
    assert!(echo_call["rank"].is_number());

    let stats = mcprouter::Stats::from_jsonl(&log);
    assert_eq!(stats.find_tools, 3);
    assert_eq!(stats.tools["beta__echo"].misses, 1);
    assert!(stats.tools["alpha_get"].def_chars > 50);
    let _ = std::fs::remove_dir_all(dir);
}
