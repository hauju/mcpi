//! Diagnoses an MCP endpoint from the outside.
//!
//! Answers the question people actually have — *"why won't this connect?"* —
//! without needing credentials, a session, or even a working server. It speaks
//! plain HTTP rather than going through `rmcp`, because half of what it reports
//! is about endpoints that are not MCP servers at all.
//!
//! Two things it deliberately does not do:
//!
//! - **It does not grade.** Every finding is a fact about the endpoint, not an
//!   opinion about the author. A diagnostic that editorialises gets argued with
//!   instead of acted on.
//! - **It does not claim to know what a given product supports.** Findings are
//!   phrased as capabilities the server offers or withholds ("no dynamic client
//!   registration"), leaving the reader to map that onto their client. Asserting
//!   "client X will fail" would be a claim about someone else's software that
//!   goes stale without warning.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod discovery;
mod endpoint;
#[cfg(test)]
mod tests;

pub use discovery::{AuthServer, ProtectedResource};

/// Whether a tool description states that the caller needs credentials.
///
/// Structural rather than a phrase list: a word of obligation *and* a word
/// about identity, both present. "Requires a connected account" matches;
/// "returns the authenticated user's projects" does not, because nothing in it
/// obliges the caller.
///
/// This reads prose nobody wrote for us, so everything downstream treats it as
/// a hint. It lives here because the endpoint check and the desktop app must
/// agree about what counts — two heuristics that drift apart would put a lock
/// on a tool the report calls open.
pub fn declares_auth(description: &str) -> bool {
    const OBLIGATION: &[&str] = &[
        "require",
        "needs a",
        "needs an",
        "need a",
        "need an",
        "must be",
        "must have",
        "only available",
        "only works",
    ];
    const IDENTITY: &[&str] = &[
        "account",
        "api key",
        "apikey",
        "access token",
        "oauth",
        "authenticat",
        "signed in",
        "sign in",
        "logged in",
        "credential",
        "subscription",
    ];
    let text = description.to_lowercase();
    OBLIGATION.iter().any(|w| text.contains(w)) && IDENTITY.iter().any(|w| text.contains(w))
}

/// How long any single request gets. Generous enough for a cold serverless
/// start, short enough that a diagnosis of an unreachable host still returns.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing, nothing wrong.
    Note,
    /// Works, but will surprise someone.
    Warning,
    /// Something cannot connect because of this.
    Blocker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// The verdict, readable on its own.
    pub title: String,
    /// Why, and what to do about it.
    pub detail: String,
}

impl Finding {
    fn new(severity: Severity, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            severity,
            title: title.into(),
            detail: detail.into(),
        }
    }
}

/// What the URL turned out to be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointKind {
    /// Speaks MCP over Streamable HTTP.
    Mcp,
    /// Speaks only the deprecated 2024-11-05 HTTP+SSE transport: a GET opens
    /// an event stream, but a Streamable HTTP POST cannot initialize.
    LegacySse,
    /// An HTML page. `candidates` are MCP-looking URLs found in its markup,
    /// because a documentation page usually links to the endpoint it documents.
    WebPage { candidates: Vec<String> },
    /// Reachable, but not something this client can talk to.
    Unexpected { status: u16, content_type: String },
    /// The request never completed.
    Unreachable { error: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub url: String,
    pub kind: Option<EndpointKind>,
    pub protocol_version: Option<String>,
    pub server: Option<Identity>,
    pub capabilities: Option<Value>,
    /// Server instructions, which often explain the auth model in prose.
    pub instructions: Option<String>,
    /// Whether `initialize` succeeded with no credentials at all.
    pub open_to_anonymous: bool,
    /// Milliseconds until the endpoint answered the initialize POST — time to
    /// response headers for that one request, not the whole probe. `None` when
    /// no response ever arrived, so `Some` is also the fact "it responded".
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// Tools listable without credentials. Empty is meaningful: it can mean a
    /// server whose whole surface is gated.
    pub public_tools: Vec<String>,
    /// The subset of `public_tools` whose own descriptions say they need an
    /// account or a key. Listed anonymously, callable only with credentials —
    /// a requirement stated in prose rather than in the protocol.
    #[serde(default)]
    pub gated_tools: Vec<String>,
    /// The `WWW-Authenticate` header, when the server issued a challenge.
    pub challenge: Option<String>,
    pub protected_resource: Option<ProtectedResource>,
    pub authorization_server: Option<AuthServer>,
    pub findings: Vec<Finding>,
}

impl Report {
    /// The worst thing found, for a one-glance verdict.
    pub fn severity(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }

    pub fn blockers(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
    }
}

/// Diagnose an endpoint.
///
/// Never fails: an unreachable host is a finding, not an error. The whole point
/// is to return a verdict on input that does not work.
pub async fn probe(url: &str) -> Report {
    let mut report = Report {
        url: url.to_string(),
        ..Default::default()
    };

    let client = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("mcpi/probe")
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            report.kind = Some(EndpointKind::Unreachable {
                error: e.to_string(),
            });
            report.findings.push(Finding::new(
                Severity::Blocker,
                "Could not build an HTTP client",
                e.to_string(),
            ));
            return report;
        }
    };

    if url::Url::parse(url).is_err() {
        report.kind = Some(EndpointKind::Unreachable {
            error: "not a valid URL".into(),
        });
        report.findings.push(Finding::new(
            Severity::Blocker,
            "That is not a valid URL",
            "An MCP endpoint looks like https://example.com/mcp.",
        ));
        return report;
    }

    endpoint::identify(&client, url, &mut report).await;

    // Only an endpoint that actually speaks MCP is worth preflighting — for a
    // web page or a dead URL the CORS answer says nothing.
    if matches!(report.kind, Some(EndpointKind::Mcp)) {
        endpoint::check_cors(&client, url, &mut report).await;
    }

    // Discovery runs regardless of how identification went. A server that
    // answers `initialize` anonymously can still gate most of its surface, and
    // its metadata is the only way to find that out before signing in.
    discovery::resolve(&client, url, &mut report).await;

    derive_findings(&mut report);
    report
        .findings
        .sort_by_key(|f| std::cmp::Reverse(f.severity));
    report
}

fn derive_findings(report: &mut Report) {
    let Some(auth) = report.authorization_server.as_ref() else {
        // No authorization server: either genuinely open, or gated by a
        // credential the OAuth metadata cannot describe.
        if report.challenge.is_some() {
            report.findings.push(Finding::new(
                Severity::Warning,
                "Authentication is required, but not via OAuth",
                "The server challenged for credentials without pointing at any OAuth metadata, \
                 so it most likely expects a bearer token or API key you have to obtain by hand. \
                 Add it as a header when saving the server.",
            ));
        } else if report.open_to_anonymous {
            report.findings.push(Finding::new(
                Severity::Note,
                "No authentication",
                "The server answered without credentials. Anything that can reach the URL can \
                 use it.",
            ));
        }
        return;
    };

    // How a client obtains a client id — the fact people actually need before
    // they can connect. Reported as the capability the server offers, never as
    // a prediction about which client will fail on it: these findings sit on
    // public listings for other people's servers, and a scheme this crate has
    // not heard of is not a defect.
    match (
        &auth.registration_endpoint,
        auth.client_id_metadata_document,
    ) {
        (Some(endpoint), _) => report.findings.push(Finding::new(
            Severity::Note,
            "Dynamic client registration is available",
            format!(
                "Clients can register themselves at {endpoint} (RFC 7591), which is what most \
                 sign-in flows rely on."
            ),
        )),
        // A Note, not a Blocker. This server offers a working way in — a newer
        // one. Grading it as broken put a red pill on healthy deployments and
        // asserted what someone else's client would do, which goes stale
        // without warning.
        (None, true) => report.findings.push(Finding::new(
            Severity::Note,
            "Registration is by client-ID metadata document",
            "The authorization server offers no registration endpoint (RFC 7591). It advertises \
             `client_id_metadata_document_supported` instead: the client_id is a URL to a \
             document describing the client, so no registration step is needed by a client that \
             implements it.",
        )),
        // A Warning, not a Blocker: it works, with a credential obtained
        // elsewhere. Plenty of healthy commercial servers issue client ids
        // from a dashboard, and "cannot connect" is untrue of every one of
        // them.
        (None, false) => report.findings.push(Finding::new(
            Severity::Warning,
            "A client id must be issued out of band",
            "The authorization server offers neither dynamic client registration (RFC 7591) nor \
             client-ID metadata documents, so a client id has to be obtained some other way — \
             usually a dashboard or a support request — before an authorization-code flow can \
             start.",
        )),
    }

    if !auth.code_challenge_methods.iter().any(|m| m == "S256") {
        report.findings.push(Finding::new(
            Severity::Warning,
            "PKCE S256 is not advertised",
            "The MCP authorization spec requires PKCE with S256, and this server's metadata does \
             not list it among its `code_challenge_methods_supported`.",
        ));
    }

    if report.protected_resource.is_none() {
        report.findings.push(Finding::new(
            Severity::Warning,
            "No protected-resource metadata",
            "RFC 9728 metadata is how a client discovers which authorization server guards this \
             endpoint. Without it, discovery depends on guessing well-known paths.",
        ));
    }

    if report.open_to_anonymous && !report.public_tools.is_empty() {
        report.findings.push(Finding::new(
            Severity::Note,
            format!(
                "{} tool{} usable without signing in",
                report.public_tools.len(),
                if report.public_tools.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            format!(
                "The server exposes a public surface before authorization: {}.",
                report.public_tools.join(", ")
            ),
        ));
    }
}
