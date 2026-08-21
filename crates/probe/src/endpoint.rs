//! Working out what a URL actually is.

use serde_json::{Value, json};

use crate::{EndpointKind, Finding, Identity, Report, Severity};

/// The initialize request every MCP client opens with. Sent verbatim so the
/// answer is the one a real client would get.
fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "mcpi-probe", "version": "1" },
        },
    })
}

pub(crate) async fn identify(client: &reqwest::Client, url: &str, report: &mut Report) {
    let started = std::time::Instant::now();
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&initialize_body())
        .send()
        .await;

    let response = match response {
        Ok(response) => {
            report.latency_ms = Some(started.elapsed().as_millis() as u64);
            response
        }
        Err(e) => {
            report.kind = Some(EndpointKind::Unreachable {
                error: e.to_string(),
            });
            report.findings.push(Finding::new(
                Severity::Blocker,
                "Could not reach the endpoint",
                e.to_string(),
            ));
            return;
        }
    };

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    report.challenge = response
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Stateful servers hand out a session on `initialize` and refuse later
    // requests that do not echo it.
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let body = response.text().await.unwrap_or_default();

    // An HTML body is the single most common way this goes wrong: the
    // documentation page and the endpoint live at adjacent paths, and the URL
    // people copy is the one they were reading.
    if content_type.contains("text/html") || body.trim_start().starts_with('<') {
        // The page itself has to be fetched with GET. A route that only serves
        // HTML commonly answers POST with an empty `405`, so the body in hand
        // is nothing — and the links that name the real endpoint are in the
        // document a browser would have loaded.
        let page = if body.trim().is_empty() {
            client
                .get(url)
                .send()
                .await
                .ok()
                .map(|r| async move { r.text().await.unwrap_or_default() })
        } else {
            None
        };
        let page = match page {
            Some(fetch) => fetch.await,
            None => body,
        };

        let candidates = mcp_candidates(&page, url);
        report.kind = Some(EndpointKind::WebPage {
            candidates: candidates.clone(),
        });
        report.findings.push(Finding::new(
            Severity::Blocker,
            "This is a web page, not an MCP endpoint",
            if candidates.is_empty() {
                format!(
                    "The URL served HTML rather than JSON-RPC (POST returned {status}). It is \
                     probably documentation about the server rather than the server itself."
                )
            } else {
                format!(
                    "The URL served HTML rather than JSON-RPC (POST returned {status}) — almost \
                     certainly the documentation page, not the endpoint. It names: {}. Try one of \
                     those.",
                    candidates.join(", ")
                )
            },
        ));
        return;
    }

    if status == 401 {
        report.kind = Some(EndpointKind::Mcp);
        report.findings.push(Finding::new(
            Severity::Note,
            "Authentication is required to connect",
            "The server rejected an anonymous initialize request, so nothing is visible before \
             signing in.",
        ));
        return;
    }

    let Some(json) = json_payload(&body) else {
        // A POST that went nowhere plus a GET that streams is the deprecated
        // 2024-11-05 HTTP+SSE transport — a current-spec server would have
        // answered the POST, so the combination is unambiguous.
        if sniffs_as_legacy_sse(client, url).await {
            report.kind = Some(EndpointKind::LegacySse);
            report.findings.push(Finding::new(
                Severity::Blocker,
                "Speaks only the deprecated HTTP+SSE transport",
                "A GET opened an event stream where a POST could not initialize: the 2024-11-05 \
                 transport, replaced by Streamable HTTP in protocol revision 2025-03-26. Clients \
                 built on the current spec cannot connect unless they ship a legacy fallback.",
            ));
            return;
        }
        report.kind = Some(EndpointKind::Unexpected {
            status,
            content_type: content_type.clone(),
        });
        report.findings.push(Finding::new(
            Severity::Blocker,
            format!("The endpoint answered {status}, not MCP"),
            format!(
                "Expected a JSON-RPC response; got {} bytes of {}. Streamable HTTP endpoints \
                 accept a POST at the same URL a client connects to.",
                body.len(),
                if content_type.is_empty() {
                    "an unlabelled body"
                } else {
                    &content_type
                }
            ),
        ));
        return;
    };

    if let Some(error) = json.get("error") {
        report.kind = Some(EndpointKind::Mcp);
        report.findings.push(Finding::new(
            Severity::Warning,
            "The server refused to initialize",
            format!("It speaks JSON-RPC but rejected the handshake: {error}"),
        ));
        return;
    }

    let Some(result) = json.get("result") else {
        report.kind = Some(EndpointKind::Unexpected {
            status,
            content_type,
        });
        report.findings.push(Finding::new(
            Severity::Blocker,
            "The response was not an MCP handshake",
            "The endpoint returned JSON with neither a result nor an error field.",
        ));
        return;
    };

    report.kind = Some(EndpointKind::Mcp);
    report.open_to_anonymous = true;
    report.protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .map(str::to_string);
    report.capabilities = result.get("capabilities").cloned();
    report.instructions = result
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string);
    report.server = result.get("serverInfo").map(|info| Identity {
        name: string_at(info, "name"),
        version: string_at(info, "version"),
        title: info
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
    });

    let tools = list_public_tools(client, url, session.as_deref()).await;
    report.gated_tools = tools
        .iter()
        .filter(|(_, description)| crate::declares_auth(description))
        .map(|(name, _)| name.clone())
        .collect();
    report.public_tools = tools.into_iter().map(|(name, _)| name).collect();

    // A server that answers anonymously, lists tools, and never challenges has
    // put its credential requirement in prose instead of in the protocol. The
    // requirement is real either way; what is missing is any signal a client
    // can act on, so the refusal arrives as a tool error after the call rather
    // than as an authorization step before it.
    if !report.gated_tools.is_empty() && report.challenge.is_none() {
        let count = report.gated_tools.len();
        report.findings.push(Finding::new(
            Severity::Warning,
            "Tools needing credentials are listed without a challenge",
            format!(
                "{count} of the {total} tools listed anonymously say in their own descriptions                  that they need an account or a key ({names}), but the endpoint issued no                  WWW-Authenticate challenge. The requirement is stated only in prose, so a call                  reaches the tool and comes back as a tool error instead of an authorization step.",
                total = report.public_tools.len(),
                names = report.gated_tools.join(", "),
            ),
        ));
    }
}

/// Whether a cross-origin preflight would let a browser in. Native clients
/// never send one, so the finding is a note about reach, not a defect: an
/// endpoint without CORS headers is invisible to anything running in a web
/// page, and the failure there is silent — the browser blocks it, the server
/// never sees it, and no error names the cause.
pub(crate) async fn check_cors(client: &reqwest::Client, url: &str, report: &mut Report) {
    let response = client
        .request(reqwest::Method::OPTIONS, url)
        .header("origin", "https://example.com")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "content-type, mcp-protocol-version",
        )
        .send()
        .await;
    let Ok(response) = response else {
        return;
    };
    if response
        .headers()
        .get("access-control-allow-origin")
        .is_none()
    {
        report.findings.push(Finding::new(
            Severity::Note,
            "No CORS headers for browser-based clients",
            "A cross-origin preflight came back without Access-Control-Allow-Origin, so the \
             endpoint is reachable from native clients only — code running in a web page is \
             stopped by the browser before a request is ever sent.",
        ));
    }
}

/// Whether a GET on the URL opens an event stream. Only the headers are read
/// — an SSE stream never ends, and the content type alone identifies the
/// transport; dropping the response aborts the connection.
async fn sniffs_as_legacy_sse(client: &reqwest::Client, url: &str) -> bool {
    let Ok(response) = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await
    else {
        return false;
    };
    response.status().is_success()
        && response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"))
}

/// The JSON-RPC payload of a POST response: the body itself when it is plain
/// JSON, or the first `data:` frame that parses when the server answered with
/// an SSE stream. Streamable HTTP allows either shape for a POST response, and
/// servers that stream commonly prepend an empty priming event — so the frame
/// that parses is the payload, not simply the first one.
fn json_payload(body: &str) -> Option<Value> {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        return Some(json);
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .find_map(|frame| serde_json::from_str::<Value>(frame.trim()).ok())
}

/// What a client could call before authorizing.
/// Anonymously listable tools as `(name, description)`. The description is
/// kept because the only statement a server makes about needing credentials is
/// often in there, and an empty one simply never matches.
async fn list_public_tools(
    client: &reqwest::Client,
    url: &str,
    session: Option<&str>,
) -> Vec<(String, String)> {
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }

    let Ok(response) = request.send().await else {
        return Vec::new();
    };
    let Some(json) = response.text().await.ok().as_deref().and_then(json_payload) else {
        return Vec::new();
    };

    json.get("result")
        .and_then(|r| r.get("tools"))
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?;
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Some((name.to_string(), description.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// MCP-looking URLs mentioned in an HTML page, resolved against its own address.
///
/// Crude on purpose — the goal is "did you mean this one?", and a documentation
/// page almost always names the endpoint it documents.
pub(crate) fn mcp_candidates(html: &str, page_url: &str) -> Vec<String> {
    let Ok(base) = url::Url::parse(page_url) else {
        return Vec::new();
    };

    let mut found: Vec<String> = Vec::new();

    // Scanning for URL *starts* rather than splitting on delimiters. A page
    // built by a modern framework embeds its links backslash-escaped inside a
    // JSON payload (`\"https://host/api/mcp\"`), so a token produced by
    // splitting begins with a backslash and never matches a scheme.
    let mut tokens: Vec<String> = Vec::new();
    for scheme in ["https://", "http://"] {
        let mut cursor = 0;
        while let Some(offset) = html[cursor..].find(scheme) {
            let start = cursor + offset;
            tokens.push(url_run(&html[start..]));
            cursor = start + scheme.len();
        }
    }
    // Relative links, which only survive inside quotes.
    for quoted in html.split(['"', '\'']) {
        if quoted.starts_with('/') && !quoted.contains(' ') {
            tokens.push(quoted.trim_end_matches('\\').to_string());
        }
    }

    for token in tokens {
        // `&` is legal in a URL but far more often the start of an escaped
        // entity in rendered markup (`…/api/mcp&quot;`), so a token is cut at
        // the first one that opens like one.
        let token = ["&quot", "&amp", "&lt", "&gt", "&nbsp", "&#"]
            .iter()
            .filter_map(|entity| token.find(entity))
            .min()
            .map_or(token.as_str(), |cut| &token[..cut]);
        let token = token.trim_end_matches(['.', ',', ';', '\\']);
        if !token.contains("mcp") {
            continue;
        }
        let absolute = if token.starts_with("http://") || token.starts_with("https://") {
            token.to_string()
        } else if token.starts_with('/') {
            match base.join(token) {
                Ok(joined) => joined.to_string(),
                Err(_) => continue,
            }
        } else {
            continue;
        };

        // The page's own address is not a suggestion.
        if absolute.trim_end_matches('/') == page_url.trim_end_matches('/') {
            continue;
        }
        // Static assets mentioning "mcp" are noise. Tested against the path
        // rather than the whole URL, because a bundler appends a cache-busting
        // query and `…/page-6dab.js?dpl=…` does not end in `.js`.
        let path = url::Url::parse(&absolute)
            .map(|parsed| parsed.path().to_string())
            .unwrap_or_else(|_| absolute.clone());
        if [
            ".js", ".css", ".png", ".svg", ".ico", ".woff2", ".map", ".json",
        ]
        .iter()
        .any(|extension| path.ends_with(extension))
        {
            continue;
        }
        if !found.contains(&absolute) {
            found.push(absolute);
        }
    }

    found.truncate(4);
    found
}

/// The URL at the start of `text`, ending at the first character that cannot
/// appear in one unescaped.
fn url_run(text: &str) -> String {
    text.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || "-._~:/?#@!$&*+,;=%".contains(*c))
        .collect()
}

fn string_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
