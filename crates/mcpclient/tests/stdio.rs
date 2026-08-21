//! Drives the real `mockserver` binary over stdio.
//!
//! Nothing here is mocked: a child process is spawned, a genuine MCP handshake
//! runs over its pipes, and the assertions are on what came back. That makes
//! this the test that would actually catch an rmcp upgrade breaking the client.

use std::collections::BTreeMap;
use std::path::PathBuf;

use mcpclient::{Error, Event, Handle, Transport};

/// Locate the fixture binary next to this test executable.
///
/// `CARGO_BIN_EXE_*` only covers binaries in the *same* package, and
/// `mockserver` is a workspace sibling shared with the app, so it is found by
/// walking up out of `target/<profile>/deps/`.
fn mockserver_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("test executable has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let binary = dir.join("mockserver");
    assert!(
        binary.exists(),
        "fixture not built at {}. Run `cargo build -p mockserver` first \
         (`just test` and CI both do this).",
        binary.display()
    );
    binary
}

fn fixture(variant: &str) -> Transport {
    Transport::Stdio {
        command: mockserver_binary().to_string_lossy().into_owned(),
        args: vec!["--variant".into(), variant.into()],
        env: BTreeMap::new(),
        cwd: None,
    }
}

#[tokio::test]
async fn connects_and_reports_who_the_server_is() {
    let (handle, info) = Handle::connect(&fixture("a"))
        .await
        .expect("fixture should connect");

    let server = info
        .server_info
        .as_ref()
        .expect("fixture identifies itself");
    assert_eq!(server.name, "mockserver");
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.resources.is_some());
    assert!(info.capabilities.prompts.is_some());

    handle.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn lists_tools_resources_and_prompts() {
    let (handle, _) = Handle::connect(&fixture("a")).await.unwrap();

    let mut tools: Vec<String> = handle
        .list_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    tools.sort();
    assert_eq!(tools, ["deprecated_tool", "echo", "search"]);

    let resources = handle.list_resources().await.expect("resources/list");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "mock://notes");
    assert_eq!(resources[0].mime_type.as_deref(), Some("text/markdown"));

    let prompts = handle.list_prompts().await.expect("prompts/list");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "review");

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn calls_a_tool_and_reads_the_result_back() {
    let (handle, _) = Handle::connect(&fixture("a")).await.unwrap();

    let mut arguments = serde_json::Map::new();
    arguments.insert("message".into(), "round trip".into());
    let result = handle
        .call_tool("echo", arguments)
        .await
        .expect("tools/call should succeed");

    assert_ne!(result.is_error, Some(true));
    let text = serde_json::to_string(&result.content).unwrap();
    assert!(text.contains("round trip"), "unexpected content: {text}");

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn reads_a_resource_and_gets_a_prompt() {
    let (handle, _) = Handle::connect(&fixture("a")).await.unwrap();

    let resource = handle
        .read_resource("mock://notes")
        .await
        .expect("resources/read");
    let body = serde_json::to_string(&resource.contents).unwrap();
    assert!(body.contains("Fixture body"), "unexpected body: {body}");

    let prompt = handle
        .get_prompt("review", None)
        .await
        .expect("prompts/get");
    assert_eq!(prompt.messages.len(), 1);

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_error_surfaces_as_a_request_error() {
    let (handle, _) = Handle::connect(&fixture("a")).await.unwrap();

    let error = handle
        .call_tool("no_such_tool", serde_json::Map::new())
        .await
        .expect_err("calling a tool that does not exist should fail");
    assert!(matches!(error, Error::Request(_)), "got {error:?}");

    handle.shutdown().await.unwrap();
}

/// The payoff test: two live servers, snapshotted and diffed, producing exactly
/// the classifications `schemadiff`'s unit tests describe. This is what the
/// product does, end to end, minus the UI.
#[tokio::test]
async fn snapshots_of_the_two_variants_diff_into_classified_changes() {
    let (a, _) = Handle::connect(&fixture("a")).await.unwrap();
    let before = a.snapshot().await.expect("snapshot of variant a");
    a.shutdown().await.unwrap();

    let (b, _) = Handle::connect(&fixture("b")).await.unwrap();
    let after = b.snapshot().await.expect("snapshot of variant b");
    b.shutdown().await.unwrap();

    assert_ne!(
        before.digest_source(),
        after.digest_source(),
        "the variants must differ, or this fixture is broken"
    );

    let diff = schemadiff::diff(&before, &after);
    assert_eq!(diff.severity(), Some(schemadiff::Severity::Breaking));

    let breaking = diff.breaking_items();
    assert!(breaking.contains(&"search"), "search got stricter");
    assert!(breaking.contains(&"deprecated_tool"), "it was removed");
    assert!(breaking.contains(&"mock://notes"), "MIME type changed");
    assert!(breaking.contains(&"review"), "required argument added");

    // A tool that did not move must not appear at all.
    let touched: Vec<&str> = diff
        .tools
        .iter()
        .map(schemadiff::ItemChange::name)
        .collect();
    assert!(
        !touched.contains(&"echo"),
        "echo is identical in both variants but was reported: {touched:?}"
    );

    // And the new tool is an addition, not a break.
    assert!(matches!(
        diff.tools.iter().find(|c| c.name() == "summarize"),
        Some(schemadiff::ItemChange::Added { .. })
    ));
}

/// The liveness edge: a child that dies mid-session must announce itself
/// rather than leave the session reading as connected until a later call
/// happens to fail.
#[tokio::test]
async fn a_crashed_child_announces_the_disconnect() {
    let (handle, _) = Handle::connect(&fixture("a")).await.unwrap();
    let mut events = handle.events();

    // `die` is the fixture's unadvertised escape hatch: it exits without
    // replying, exactly like a server crashing under load.
    let _ = handle.call_tool("die", serde_json::Map::new()).await;

    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("expected a Disconnected event, got silence")
            .expect("the event channel closed before announcing the death");
        if let Event::Disconnected { .. } = event {
            return;
        }
    }
}

#[tokio::test]
async fn a_missing_command_says_where_it_looked() {
    let transport = Transport::Stdio {
        command: "definitely-not-a-real-mcp-server".into(),
        args: vec![],
        env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        cwd: None,
    };

    let error = Handle::connect(&transport)
        .await
        .expect_err("a missing binary cannot connect");

    // The whole point of resolving PATH ourselves is that this message is
    // actionable instead of a bare "No such file or directory".
    match error {
        Error::CommandNotFound { command, searched } => {
            assert_eq!(command, "definitely-not-a-real-mcp-server");
            assert_eq!(searched, "/usr/bin:/bin");
        }
        other => panic!("expected CommandNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn the_handshake_is_kept_even_though_nothing_could_subscribe_yet() {
    // The transcript's whole value is that it starts at `initialize`. A caller
    // cannot subscribe until `connect` returns, so if the backlog were not
    // captured inside `connect` these frames would be unrecoverable.
    let (result, backlog) = Handle::connect_verbose(&fixture("a")).await;
    let (handle, _) = result.unwrap();

    let frames: Vec<&serde_json::Value> = backlog
        .iter()
        .filter_map(|event| match event {
            Event::Wire { message, .. } => Some(message),
            _ => None,
        })
        .collect();

    assert!(
        frames
            .iter()
            .any(|m| m["method"] == "initialize" && m["id"].is_number()),
        "expected the initialize request in the backlog, got {frames:#?}"
    );
    assert!(
        backlog.iter().any(|event| matches!(
            event,
            Event::Stderr { line } if line.contains("mockserver")
        )),
        "expected the fixture's startup banner on stderr"
    );

    handle.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn a_tool_call_shows_up_in_the_transcript_both_ways() {
    let (handle, _) = Handle::connect(&fixture("a")).await.unwrap();
    let mut events = handle.events();

    let mut arguments = serde_json::Map::new();
    arguments.insert("message".into(), serde_json::json!("hi"));
    handle
        .call_tool("echo", arguments)
        .await
        .expect("echo should answer");

    let (mut sent, mut received) = (false, false);
    while let Ok(event) = events.try_recv() {
        if let Event::Wire { outbound, message } = event {
            if outbound && message["method"] == "tools/call" {
                assert_eq!(message["params"]["name"], "echo");
                sent = true;
            }
            // The response carries no method, so it is matched by its id
            // pairing with a result rather than by name.
            if !outbound && message["result"].is_object() {
                received = true;
            }
        }
    }
    assert!(sent, "the outgoing tools/call was not recorded");
    assert!(received, "the incoming result was not recorded");

    handle.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn a_failed_spawn_still_yields_whatever_the_child_managed_to_say() {
    // The whole point of capturing stderr: a connect that fails leaves no
    // handle to ask afterwards, and the child's own complaint is the only
    // account of why.
    let transport = Transport::Stdio {
        command: mockserver_binary().to_string_lossy().into_owned(),
        args: vec!["--variant".into(), "nonsense".into()],
        env: BTreeMap::new(),
        cwd: None,
    };

    let (result, backlog) = Handle::connect_verbose(&transport).await;
    assert!(result.is_err(), "an unknown variant should not connect");
    assert!(
        backlog.iter().any(|event| matches!(
            event,
            Event::Stderr { line } if line.contains("unknown variant")
        )),
        "expected the child's own error on stderr, got {backlog:#?}"
    );
}
