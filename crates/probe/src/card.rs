//! Finding the Server Card that describes an endpoint before anyone connects.
//!
//! SEP-2127 splits discovery in two. A domain publishes an AI Catalog at
//! `/.well-known/ai-catalog.json` whose entries link to — or embed — Server
//! Cards; a card names the server and the remote endpoints it answers at.
//! Cards may live anywhere the catalog points, with `<endpoint>/server-card`
//! reserved as the default. Cards deliberately carry no tools, resources, or
//! prompts, so what can be checked against the live server is identity,
//! transport, and protocol version — and only those are.
//!
//! A card is advisory by the spec's own word: when it and the handshake
//! disagree the handshake wins, and the disagreement is the finding.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EndpointKind, Finding, Report, Severity};

/// The media type a catalog entry must carry to be a Server Card.
const CARD_TYPE: &str = "application/mcp-server-card+json";

/// Cap on cards fetched from one catalog. A catalog names what it likes; this
/// bounds how much fetching a single probe can be talked into.
pub(crate) const MAX_CARDS: usize = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerCard {
    /// Where the card was read — the catalog's URL for a card embedded in it.
    pub card_url: String,
    /// Reverse-DNS, `com.example/weather`. Not compared against `serverInfo`,
    /// whose name has no such convention, so a mismatch would be noise.
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub remotes: Vec<Remote>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    /// `streamable-http` or `sse`, as the card spells it.
    pub transport: String,
    pub url: String,
    pub protocol_versions: Vec<String>,
}

/// One MCP entry of an AI Catalog: a link to a card, or the card itself.
pub(crate) enum Entry {
    Linked(String),
    Inline(ServerCard),
}

/// The catalog URL for the origin of `url`.
pub(crate) fn catalog_url(url: &url::Url) -> String {
    let mut origin = url.clone();
    origin.set_path("/.well-known/ai-catalog.json");
    origin.set_query(None);
    origin.set_fragment(None);
    origin.to_string()
}

/// The MCP entries of the catalog at `url`, or `None` when nothing there
/// parses as a catalog. An empty list is a catalog with no MCP servers in it.
pub(crate) async fn catalog_entries(client: &httpguard::Client, url: &str) -> Option<Vec<Entry>> {
    let json = fetch_json(client, url, "application/ai-catalog+json, application/json").await?;
    let entries = json.get("entries")?.as_array()?;
    Some(
        entries
            .iter()
            .filter(|entry| entry.get("type").and_then(Value::as_str) == Some(CARD_TYPE))
            .filter_map(|entry| {
                if let Some(data) = entry.get("data") {
                    parse(data, url).map(Entry::Inline)
                } else {
                    let link = entry.get("url").and_then(Value::as_str)?;
                    let link = url::Url::parse(url).ok()?.join(link).ok()?;
                    Some(Entry::Linked(link.to_string()))
                }
            })
            .take(MAX_CARDS)
            .collect(),
    )
}

pub(crate) async fn fetch_card(client: &httpguard::Client, url: &str) -> Option<ServerCard> {
    let json = fetch_json(client, url, &format!("{CARD_TYPE}, application/json")).await?;
    parse(&json, url)
}

/// A card needs a name and a version to be one; everything else is optional,
/// and a remote whose fields do not parse is dropped rather than guessed at.
fn parse(json: &Value, card_url: &str) -> Option<ServerCard> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let remotes = json
        .get("remotes")
        .and_then(Value::as_array)
        .map(|remotes| {
            remotes
                .iter()
                .filter_map(|remote| {
                    Some(Remote {
                        transport: text(remote, "type")?,
                        url: text(remote, "url")?,
                        protocol_versions: remote
                            .get("supportedProtocolVersions")
                            .and_then(Value::as_array)
                            .map(|versions| {
                                versions
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ServerCard {
        card_url: card_url.to_string(),
        name: text(json, "name")?,
        version: text(json, "version")?,
        title: text(json, "title"),
        description: text(json, "description"),
        remotes,
    })
}

impl ServerCard {
    /// The remote entry for `url`, if the card lists it. Templated URLs
    /// (`https://{tenant}.example.com/mcp`) cannot be matched without the
    /// variables, so they never are.
    pub fn remote_for(&self, url: &str) -> Option<&Remote> {
        self.remotes
            .iter()
            .find(|remote| !remote.url.contains('{') && same_endpoint(&remote.url, url))
    }
}

/// Equal up to a trailing slash, which servers and documentation add and drop
/// freely for the same endpoint.
pub(crate) fn same_endpoint(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Look for the card describing the endpoint `url`: the reserved
/// `<url>/server-card` first, then the origin's catalog. A catalog card for
/// some other endpoint is not this endpoint's card and is ignored.
pub(crate) async fn resolve(client: &httpguard::Client, url: &str, report: &mut Report) {
    let Ok(endpoint) = url::Url::parse(url) else {
        return;
    };
    let reserved = format!("{}/server-card", url.trim_end_matches('/'));
    if let Some(card) = fetch_card(client, &reserved).await {
        report.server_card = Some(card);
        return;
    }
    let Some(entries) = catalog_entries(client, &catalog_url(&endpoint)).await else {
        return;
    };
    for entry in entries {
        let card = match entry {
            Entry::Inline(card) => Some(card),
            Entry::Linked(link) => fetch_card(client, &link).await,
        };
        if let Some(card) = card.filter(|card| card.remote_for(url).is_some()) {
            report.server_card = Some(card);
            return;
        }
    }
}

/// Where the card and the handshake disagree, each as a sentence. Only facts
/// both sides actually stated are compared — a card that omits protocol
/// versions has not contradicted anything.
pub fn mismatches(card: &ServerCard, report: &Report) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(server) = report.server.as_ref().filter(|s| !s.version.is_empty())
        && server.version != card.version
    {
        found.push(format!(
            "the card says version {}, the server says {}",
            card.version, server.version
        ));
    }
    let Some(remote) = card.remote_for(&report.url) else {
        return found;
    };
    let transport = match report.kind {
        Some(EndpointKind::Mcp) => Some("streamable-http"),
        Some(EndpointKind::LegacySse) => Some("sse"),
        _ => None,
    };
    if let Some(transport) = transport
        && remote.transport != transport
    {
        found.push(format!(
            "the card lists the {} transport, the endpoint speaks {transport}",
            remote.transport
        ));
    }
    if let Some(version) = &report.protocol_version
        && !remote.protocol_versions.is_empty()
        && !remote.protocol_versions.contains(version)
    {
        found.push(format!(
            "the server negotiated protocol {version}, which the card's list ({}) leaves out",
            remote.protocol_versions.join(", ")
        ));
    }
    found
}

pub(crate) fn derive_findings(report: &mut Report) {
    if !matches!(
        report.kind,
        Some(EndpointKind::Mcp | EndpointKind::LegacySse)
    ) {
        return;
    }
    let Some(card) = report.server_card.as_ref() else {
        report.findings.push(Finding::new(
            Severity::Note,
            "No server card",
            "Neither the reserved <endpoint>/server-card location nor the domain's \
             /.well-known/ai-catalog.json describes this endpoint (SEP-2127), so a client that \
             discovers servers by domain will not find it.",
        ));
        return;
    };
    let mismatches = mismatches(card, report);
    if mismatches.is_empty() {
        report.findings.push(Finding::new(
            Severity::Note,
            "Server card published",
            format!(
                "{} describes this endpoint, and nothing in it contradicts the handshake.",
                card.card_url
            ),
        ));
    } else {
        report.findings.push(Finding::new(
            Severity::Warning,
            "The server card contradicts the server",
            format!(
                "{}: {}. Clients that configure themselves from the card are told something the \
                 live server does not do.",
                card.card_url,
                mismatches.join("; ")
            ),
        ));
    }
}

async fn fetch_json(client: &httpguard::Client, url: &str, accept: &str) -> Option<Value> {
    let response = client.get(url).header("accept", accept).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}
