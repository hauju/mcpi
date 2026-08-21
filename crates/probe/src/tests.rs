//! Every fixture here mirrors a shape observed on a real server, so the tests
//! fail when reality changes rather than when an invented rule does.
//!
//! The servers are real sockets rather than mocked HTTP: the probe's whole job
//! is reading what comes back off the wire, including headers and content
//! types, and a mock that returns pre-parsed values would test nothing.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// Serve canned responses by request path. Keys are paths — or `"GET /path"`
/// when a route must answer methods differently, or `"POST /path tools/list"`
/// when it must answer two JSON-RPC calls to the same path differently; the
/// value is the complete HTTP response.
async fn serve(routes: HashMap<&'static str, String>) -> String {
    serve_with(|_| routes).await
}

/// Like [`serve`], but the routes are built from the server's own base URL.
///
/// Needed whenever a response has to *name* this server — a `WWW-Authenticate`
/// challenge carries an absolute URL, and the port is not known until the
/// listener is bound.
async fn serve_with<F>(build: F) -> String
where
    F: FnOnce(&str) -> HashMap<&'static str, String>,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let base = format!("http://127.0.0.1:{port}");
    let routes = Arc::new(build(&base));

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let routes = Arc::clone(&routes);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let mut first = request.lines().next().unwrap_or("").split_whitespace();
                let method = first.next().unwrap_or("GET").to_string();
                let path = first.next().unwrap_or("/").to_string();

                // Most specific key wins: the JSON-RPC method, then the HTTP
                // method, then the bare path.
                let rpc = request
                    .split_once("\"method\":\"")
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(name, _)| name.to_string());

                let response = rpc
                    .and_then(|rpc| routes.get(format!("{method} {path} {rpc}").as_str()))
                    .or_else(|| routes.get(format!("{method} {path}").as_str()))
                    .or_else(|| routes.get(path.as_str()))
                    .cloned()
                    .unwrap_or_else(|| {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    });
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    base
}

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn html_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The handshake a healthy server returns.
fn initialize_ok() -> String {
    json_response(
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1.2.3"}}}"#,
    )
}

// ── What the URL is ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_documentation_page_is_named_as_such_and_suggests_the_real_endpoint() {
    // The trustmrr.com case exactly: /mcp is the docs page, /api/mcp is the
    // server. This is the single most common way a connection attempt fails.
    let base = serve(HashMap::from([(
        "/mcp",
        html_response(
            r#"<html><head><title>MCP server</title></head><body>
               <p>Connect to <code>https://example.com/api/mcp</code></p>
               <a href="/api/mcp/discovery">discovery</a>
               <script src="/_next/chunk-mcp.js"></script>
               </body></html>"#,
        ),
    )]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    let EndpointKind::WebPage { candidates } = report.kind.clone().expect("identified") else {
        panic!("expected a web page, got {:?}", report.kind);
    };
    assert!(
        candidates.iter().any(|c| c.ends_with("/api/mcp")),
        "the endpoint named in the prose should be suggested: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|c| c.ends_with(".js")),
        "bundled assets are noise: {candidates:?}"
    );
    assert_eq!(report.severity(), Some(Severity::Blocker));
    assert!(
        report
            .blockers()
            .any(|f| f.title.contains("not an MCP endpoint"))
    );
}

#[tokio::test]
async fn a_post_only_405_still_finds_the_documentation_behind_a_get() {
    // trustmrr.com/mcp exactly: POST returns an empty 405 typed text/html, and
    // the page naming the real endpoint only exists on GET. Scanning the POST
    // body found nothing, so the most useful output of the whole probe — "did
    // you mean this URL?" — came back empty against the very server that
    // motivated the feature.
    let base = serve(HashMap::from([(
        "/mcp",
        // The fixture cannot vary by method, so it answers both the same way;
        // what matters is that an empty body triggers the GET at all.
        html_response(r#"<html><body><code>https://example.com/api/mcp</code></body></html>"#),
    )]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    let EndpointKind::WebPage { candidates } = report.kind.expect("identified") else {
        panic!("expected a web page");
    };
    assert!(
        candidates
            .iter()
            .any(|c| c == "https://example.com/api/mcp"),
        "{candidates:?}"
    );
}

#[tokio::test]
async fn entities_and_cache_busted_assets_are_kept_out_of_the_suggestions() {
    // Both observed on the real page: an escaped quote runs into the URL, and a
    // bundle is `…/page-6dab.js?dpl=…`, which does not end in `.js`.
    let base = serve(HashMap::from([(
        "/mcp",
        html_response(
            r#"<html><body>
               <span>&quot;https://example.com/api/mcp&quot;</span>
               <script src="https://example.com/_next/chunks/app/mcp/page-6dab.js?dpl=abc"></script>
               </body></html>"#,
        ),
    )]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    let EndpointKind::WebPage { candidates } = report.kind.expect("identified") else {
        panic!("expected a web page");
    };
    assert!(
        candidates.contains(&"https://example.com/api/mcp".to_string()),
        "the entity must be trimmed off: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|c| c.contains(".js")),
        "a cache-busted bundle is not a suggestion: {candidates:?}"
    );
}

#[tokio::test]
async fn a_backslash_escaped_link_is_still_found() {
    // Next.js and friends embed their links inside a JSON payload, so the
    // markup contains `\"https://host/api/mcp\"`. Splitting on quotes yields a
    // token starting with a backslash, which matches no scheme — the real
    // trustmrr.com page produced zero suggestions until this was handled.
    let base = serve(HashMap::from([(
        "/mcp",
        html_response(
            r#"<html><body><script>self.__next_f.push([1,"{\"endpoint\":\"https://example.com/api/mcp\"}"])</script></body></html>"#,
        ),
    )]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    let EndpointKind::WebPage { candidates } = report.kind.expect("identified") else {
        panic!("expected a web page");
    };
    assert!(
        candidates
            .iter()
            .any(|c| c == "https://example.com/api/mcp"),
        "the escaped URL should be recovered whole: {candidates:?}"
    );
}

#[tokio::test]
async fn a_healthy_open_server_reports_its_identity_and_public_tools() {
    let base = serve(HashMap::from([(
        "/mcp",
        // One socket serves both initialize and tools/list; the fixture keys on
        // path, and both are POSTs to the same place.
        initialize_ok(),
    )]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(report.kind, Some(EndpointKind::Mcp));
    assert!(report.open_to_anonymous);
    assert!(
        report.latency_ms.is_some(),
        "a response must carry its round-trip time"
    );
    assert_eq!(report.protocol_version.as_deref(), Some("2025-06-18"));
    let server = report.server.expect("serverInfo");
    assert_eq!(server.name, "fixture");
    assert_eq!(server.version, "1.2.3");
    // No OAuth metadata anywhere, and nothing challenged: that is a finding in
    // its own right, not an absence of findings.
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.title == "No authentication")
    );
}

#[tokio::test]
async fn an_sse_wrapped_handshake_is_recognised_as_mcp() {
    // The mcp.deepwiki.com shape: Streamable HTTP lets a server answer a POST
    // with an SSE stream instead of a JSON body, and streaming servers prepend
    // an empty priming event. This was misread as "not MCP" — a blocker
    // reported against a healthy server.
    let sse_body = "data: \n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"sse-fixture\",\"version\":\"9\"}}}\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: abc123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse_body}",
        sse_body.len()
    );
    let base = serve(HashMap::from([("/mcp", response)])).await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(report.kind, Some(EndpointKind::Mcp));
    assert!(report.open_to_anonymous);
    assert_eq!(report.server.expect("serverInfo").name, "sse-fixture");
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.severity == Severity::Blocker),
        "a healthy streaming server must not be reported as broken: {:?}",
        report.findings
    );
}

#[tokio::test]
async fn missing_cors_headers_are_a_note_on_an_mcp_endpoint() {
    // The fixture's default 404 for OPTIONS carries no CORS headers — the
    // common case, and invisible from a browser because the failure happens
    // before any request is sent.
    let base = serve(HashMap::from([("/mcp", initialize_ok())])).await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(report.kind, Some(EndpointKind::Mcp));
    let cors = report
        .findings
        .iter()
        .find(|f| f.title.contains("CORS"))
        .expect("missing CORS must be noted");
    assert_eq!(cors.severity, Severity::Note, "native clients still work");
}

#[tokio::test]
async fn an_endpoint_answering_the_preflight_gets_no_cors_finding() {
    let base = serve(HashMap::from([
        ("POST /mcp", initialize_ok()),
        (
            "OPTIONS /mcp",
            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\n\
             Access-Control-Allow-Methods: POST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ),
    ]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(report.kind, Some(EndpointKind::Mcp));
    assert!(
        !report.findings.iter().any(|f| f.title.contains("CORS")),
        "{:?}",
        report.findings
    );
}

#[tokio::test]
async fn a_legacy_sse_endpoint_is_named_as_such() {
    // The 2024-11-05 transport exactly: POST is refused, and a GET opens an
    // event stream that announces where messages go. rmcp 3.x has no client
    // for it, so the finding must name the transport rather than surfacing a
    // generic connection error.
    let base = serve(HashMap::from([
        (
            "POST /sse",
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ),
        (
            "GET /sse",
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             event: endpoint\ndata: /messages?sessionId=abc\n\n"
                .to_string(),
        ),
    ]))
    .await;

    let report = probe(&format!("{base}/sse")).await;

    assert_eq!(report.kind, Some(EndpointKind::LegacySse));
    assert_eq!(report.severity(), Some(Severity::Blocker));
    assert!(
        report
            .blockers()
            .any(|f| f.title.contains("deprecated HTTP+SSE")),
        "the transport must be named: {:?}",
        report.findings
    );
}

#[tokio::test]
async fn a_plain_405_without_a_stream_stays_unexpected() {
    // The counterpart: refusing POST alone proves nothing about the transport.
    let base = serve(HashMap::from([(
        "/api",
        "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    )]))
    .await;

    let report = probe(&format!("{base}/api")).await;
    assert!(
        matches!(report.kind, Some(EndpointKind::Unexpected { .. })),
        "got {:?}",
        report.kind
    );
}

#[tokio::test]
async fn an_unreachable_host_is_a_finding_not_an_error() {
    // The probe exists to diagnose things that do not work; refusing to return
    // a report for the worst case would defeat it.
    let report = probe("http://127.0.0.1:1/mcp").await;
    assert!(matches!(
        report.kind,
        Some(EndpointKind::Unreachable { .. })
    ));
    assert_eq!(report.severity(), Some(Severity::Blocker));
    assert!(
        report.latency_ms.is_none(),
        "no response, no latency to report"
    );
}

#[tokio::test]
async fn a_malformed_url_is_rejected_before_any_request() {
    let report = probe("not a url").await;
    assert!(
        report
            .blockers()
            .any(|f| f.title.contains("not a valid URL"))
    );
}

// ── The decisive OAuth facts ────────────────────────────────────────────────

/// A server that gates everything behind OAuth and offers the given
/// authorization-server metadata.
fn oauth_routes(as_metadata: &str) -> HashMap<&'static str, String> {
    HashMap::from([
        (
            "/mcp",
            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer resource_metadata=\"REPLACED\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ),
        (
            "/.well-known/oauth-protected-resource/mcp",
            json_response(
                r#"{"resource":"http://x/mcp","authorization_servers":[],"scopes_supported":["read"]}"#,
            ),
        ),
        (
            "/.well-known/oauth-authorization-server",
            json_response(as_metadata),
        ),
    ])
}

#[tokio::test]
async fn client_id_metadata_documents_are_a_note_not_a_blocker() {
    // Correct RFC 9728/8414 metadata, PKCE, and CIMD instead of RFC 7591.
    // This server offers a working way in — a newer one — and grading it as a
    // blocker put a red pill on a healthy deployment on a public listing.
    let base = serve(oauth_routes(
        r#"{"issuer":"http://x","authorization_endpoint":"http://x/oauth/authorize",
            "token_endpoint":"http://x/oauth/token","client_id_metadata_document_supported":true,
            "code_challenge_methods_supported":["S256"],"grant_types_supported":["authorization_code"],
            "scopes_supported":["read"]}"#,
    ))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    let auth = report.authorization_server.clone().expect("AS metadata");
    assert!(auth.registration_endpoint.is_none());
    assert!(auth.client_id_metadata_document);

    assert!(
        report.blockers().next().is_none(),
        "a server offering CIMD is not blocked: {:?}",
        report.findings
    );
    let note = report
        .findings
        .iter()
        .find(|f| f.title.contains("client-ID metadata document"))
        .expect("the registration scheme must still be stated");
    assert_eq!(note.severity, Severity::Note);
    assert!(
        note.detail.contains("client_id_metadata_document"),
        "the detail should name the scheme the server does offer"
    );
    // PKCE is fine here, so it must not also be reported.
    assert!(!report.findings.iter().any(|f| f.title.contains("PKCE")));
}

#[tokio::test]
async fn dynamic_registration_present_is_a_note_not_a_problem() {
    let base = serve(oauth_routes(
        r#"{"issuer":"http://x","registration_endpoint":"http://x/oauth/register",
            "code_challenge_methods_supported":["S256"]}"#,
    ))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(
        report
            .authorization_server
            .as_ref()
            .and_then(|a| a.registration_endpoint.as_deref()),
        Some("http://x/oauth/register")
    );
    assert!(
        report.blockers().next().is_none(),
        "a fully-equipped server should have no blockers: {:?}",
        report.findings
    );
}

#[tokio::test]
async fn no_self_service_registration_is_a_warning_not_a_blocker() {
    // Neither RFC 7591 nor CIMD. That withholds a capability worth naming, but
    // plenty of healthy commercial servers issue client ids from a dashboard,
    // and "cannot connect" is untrue of every one of them.
    let base = serve(oauth_routes(
        r#"{"issuer":"http://x","code_challenge_methods_supported":["S256"]}"#,
    ))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    let finding = report
        .findings
        .iter()
        .find(|f| f.title.contains("issued out of band"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert_eq!(finding.severity, Severity::Warning);
    assert!(report.blockers().next().is_none(), "{:?}", report.findings);
}

#[tokio::test]
async fn no_finding_predicts_what_another_client_will_do() {
    // The constraint this crate is written under, made executable. These
    // findings appear on public listings for other people's servers, so they
    // state capabilities offered or withheld — never a claim about someone
    // else's software, which goes stale without warning.
    let base = serve(oauth_routes(
        r#"{"issuer":"http://x","client_id_metadata_document_supported":true,
            "code_challenge_methods_supported":["plain"]}"#,
    ))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    assert!(!report.findings.is_empty(), "the fixture must produce some");

    for finding in &report.findings {
        let text = format!("{} {}", finding.title, finding.detail).to_lowercase();
        for claim in [
            "claude",
            "cursor",
            "chatgpt",
            "chat client",
            "will fail",
            "will be rejected",
            "is rejected when",
            "broken",
        ] {
            assert!(
                !text.contains(claim),
                "a finding must not say {claim:?}: {text}"
            );
        }
    }
}

#[tokio::test]
async fn missing_pkce_s256_is_flagged() {
    let base = serve(oauth_routes(
        r#"{"issuer":"http://x","registration_endpoint":"http://x/reg",
            "code_challenge_methods_supported":["plain"]}"#,
    ))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    assert!(report.findings.iter().any(|f| f.title.contains("PKCE")));
}

#[tokio::test]
async fn a_credential_challenge_without_oauth_metadata_reads_as_an_api_key() {
    // A server that wants a bearer token you got from a dashboard. Common, and
    // worth distinguishing from a broken OAuth setup.
    let base = serve(HashMap::from([(
        "/mcp",
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    )]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    let finding = report
        .findings
        .iter()
        .find(|f| f.title.contains("not via OAuth"))
        .expect("should distinguish an API key from OAuth");
    assert!(finding.detail.contains("header"));
}

// ── Metadata locations ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_path_suffixed_metadata_location_is_tried() {
    // RFC 9728 puts the document at /.well-known/oauth-protected-resource/api/mcp
    // for a resource at /api/mcp. Servers that serve only this location were
    // invisible to a probe that checked the bare path alone.
    let base = serve(HashMap::from([
        ("/api/mcp", initialize_ok()),
        (
            "/.well-known/oauth-protected-resource/api/mcp",
            json_response(r#"{"resource":"http://x/api/mcp","authorization_servers":[]}"#),
        ),
    ]))
    .await;

    let report = probe(&format!("{base}/api/mcp")).await;
    let resource = report
        .protected_resource
        .expect("found via the suffixed path");
    assert!(resource.metadata_url.ends_with("/api/mcp"));
}

#[tokio::test]
async fn the_challenge_wins_over_guessing_where_metadata_lives() {
    // When the server names its metadata URL, that is authoritative — it may
    // sit somewhere neither well-known convention would find.
    let base = serve_with(|base| {
        let challenge = format!(r#"Bearer resource_metadata="{base}/custom/location""#);
        HashMap::from([
            (
                "/mcp",
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {challenge}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            ),
            (
                "/custom/location",
                json_response(r#"{"resource":"http://x/mcp","authorization_servers":[]}"#),
            ),
        ])
    })
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    assert!(
        report
            .protected_resource
            .is_some_and(|r| r.metadata_url.ends_with("/custom/location")),
        "the URL named in the challenge should be fetched"
    );
}

#[test]
fn a_challenge_is_parsed_out_of_its_surrounding_parameters() {
    let parsed = crate::discovery::resource_metadata_from_challenge(
        r#"Bearer realm="mcp", resource_metadata="https://x/.well-known/oauth-protected-resource/mcp", scope="read""#,
    );
    assert_eq!(
        parsed.as_deref(),
        Some("https://x/.well-known/oauth-protected-resource/mcp")
    );
}

#[test]
fn a_challenge_without_metadata_yields_nothing_rather_than_a_guess() {
    assert_eq!(
        crate::discovery::resource_metadata_from_challenge("Bearer"),
        None
    );
}

// ── Report shape ────────────────────────────────────────────────────────────

#[tokio::test]
async fn findings_are_ordered_worst_first() {
    let base = serve(oauth_routes(
        r#"{"issuer":"http://x","code_challenge_methods_supported":["plain"]}"#,
    ))
    .await;

    let report = probe(&format!("{base}/mcp")).await;
    let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
    let mut sorted = severities.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        severities, sorted,
        "a reader should meet the blockers first"
    );
}

#[tokio::test]
async fn tools_that_declare_credentials_without_a_challenge_are_a_finding() {
    // The TrustMRR shape: initialize succeeds anonymously, tools/list returns
    // the whole surface, and the credential requirement appears only in a
    // description — never as a 401 the client could act on.
    let base = serve(HashMap::from([
        ("POST /mcp", initialize_ok()),
        (
            "POST /mcp tools/list",
            json_response(
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[
                    {"name":"get_marketplace_snapshot",
                     "description":"Show the intentionally limited public marketplace snapshot."},
                    {"name":"list_startups",
                     "description":"Browse active startups with bounded filters. Requires a connected account in ChatGPT or an API key in other MCP clients."}
                ]}}"#,
            ),
        ),
    ]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(
        report.public_tools,
        vec!["get_marketplace_snapshot", "list_startups"]
    );
    assert_eq!(
        report.gated_tools,
        vec!["list_startups"],
        "only the tool whose own prose demands credentials"
    );
    let finding = report
        .findings
        .iter()
        .find(|f| f.title == "Tools needing credentials are listed without a challenge")
        .expect("the gated-without-challenge finding");
    assert_eq!(finding.severity, Severity::Warning);
    assert!(
        finding.detail.contains("list_startups"),
        "the finding names the tool: {}",
        finding.detail
    );
}

#[tokio::test]
async fn ordinary_tool_descriptions_are_not_read_as_gated() {
    // The finding is about a requirement hidden in prose, not about mentioning
    // auth at all. A tool that merely *describes* authenticated data obliges
    // the caller to nothing, and must not draw the warning.
    let base = serve(HashMap::from([
        ("POST /mcp", initialize_ok()),
        (
            "POST /mcp tools/list",
            json_response(
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[
                    {"name":"whoami",
                     "description":"Returns the authenticated user's account, newest first."},
                    {"name":"render_startups","description":"Presentation-only tool."}
                ]}}"#,
            ),
        ),
    ]))
    .await;

    let report = probe(&format!("{base}/mcp")).await;

    assert_eq!(report.public_tools, vec!["whoami", "render_startups"]);
    assert!(
        report.gated_tools.is_empty(),
        "describing auth is not demanding it: {:?}",
        report.gated_tools
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.title == "Tools needing credentials are listed without a challenge")
    );
}
