//! Finding the OAuth metadata a client would need.
//!
//! Two RFCs are in play and both define *path-suffixed* well-known URLs, which
//! is the part servers most often get wrong: for a resource at
//! `https://host/api/mcp` the metadata belongs at
//! `https://host/.well-known/oauth-protected-resource/api/mcp`, not at the bare
//! `/.well-known/oauth-protected-resource`. Real servers serve one, the other,
//! or both, so every location is tried and the report says which answered.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Finding, Report, Severity};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtectedResource {
    /// Where this document was actually found.
    pub metadata_url: String,
    pub resource: Option<String>,
    pub authorization_servers: Vec<String>,
    pub scopes: Vec<String>,
    /// Human documentation the server points at, worth surfacing because it is
    /// usually where an API-key alternative is explained.
    pub documentation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthServer {
    pub metadata_url: String,
    pub issuer: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    /// RFC 7591. Its absence is the single most consequential fact this probe
    /// reports — a client that can only register itself has nowhere to go.
    pub registration_endpoint: Option<String>,
    /// The newer alternative, where the client id is a URL to a metadata
    /// document rather than something the server issues.
    pub client_id_metadata_document: bool,
    pub code_challenge_methods: Vec<String>,
    pub grant_types: Vec<String>,
    pub scopes: Vec<String>,
}

pub(crate) async fn resolve(client: &httpguard::Client, url: &str, report: &mut Report) {
    let Ok(endpoint) = url::Url::parse(url) else {
        return;
    };

    // A challenge naming its metadata beats guessing: the server is telling us
    // exactly where to look.
    let from_challenge = report
        .challenge
        .as_deref()
        .and_then(resource_metadata_from_challenge);

    let mut resource_urls = Vec::new();
    if let Some(named) = from_challenge {
        resource_urls.push(named);
    }
    resource_urls.extend(well_known_locations(&endpoint, "oauth-protected-resource"));

    for candidate in resource_urls {
        if let Some(found) = fetch_protected_resource(client, &candidate).await {
            report.protected_resource = Some(found);
            break;
        }
    }

    // Prefer the authorization server the resource metadata names; fall back to
    // the endpoint's own host, which is where a single-tenant server puts it.
    let mut server_urls = Vec::new();
    if let Some(resource) = &report.protected_resource {
        for issuer in &resource.authorization_servers {
            if let Ok(issuer) = url::Url::parse(issuer) {
                server_urls.extend(well_known_locations(&issuer, "oauth-authorization-server"));
                server_urls.extend(well_known_locations(&issuer, "openid-configuration"));
            }
        }
    }
    server_urls.extend(well_known_locations(
        &endpoint,
        "oauth-authorization-server",
    ));

    for candidate in server_urls {
        if let Some(found) = fetch_authorization_server(client, &candidate).await {
            report.authorization_server = Some(found);
            break;
        }
    }
}

/// Report the metadata a client would reject. Both checks are deliberately
/// looser than the RFCs: a finding here says a client that validates will fail,
/// so only what fails that way counts.
pub(crate) fn derive_findings(report: &mut Report) {
    let resource = report
        .protected_resource
        .as_ref()
        .and_then(|found| found.resource.clone())
        .filter(|resource| !resource_covers(resource, &report.url));
    if let Some(resource) = resource {
        report.findings.push(Finding::new(
            Severity::Warning,
            "The metadata names a different resource",
            format!(
                "The protected-resource metadata says it describes {resource}, which does not \
                 cover {}. RFC 9728 has clients reject metadata that names another resource, so \
                 sign-in can fail before it starts.",
                report.url
            ),
        ));
    }

    let Some(auth) = report.authorization_server.as_ref() else {
        return;
    };
    let expected = expected_issuers(report);
    if !expected
        .iter()
        .any(|issuer| same_issuer(issuer, &auth.issuer))
    {
        report.findings.push(Finding::new(
            Severity::Warning,
            "The issuer does not match",
            format!(
                "The authorization server calls itself {}, but a client reached it as {}. \
                 RFC 8414 has clients reject metadata whose issuer differs from the one they \
                 asked for, so sign-in can fail before it starts.",
                auth.issuer,
                expected.join(" or ")
            ),
        ));
    }
}

/// Whether a client at `endpoint` can accept metadata naming `resource`: same
/// origin, and the endpoint is that path or beneath it. A resource naming the
/// origin covers everything on it.
fn resource_covers(resource: &str, endpoint: &str) -> bool {
    let Ok(endpoint) = url::Url::parse(endpoint) else {
        return true;
    };
    let Ok(resource) = url::Url::parse(resource) else {
        return false;
    };
    let resource_path = resource.path().trim_end_matches('/');
    let endpoint_path = endpoint.path().trim_end_matches('/');
    resource.origin() == endpoint.origin()
        && (endpoint_path == resource_path
            || endpoint_path.starts_with(&format!("{resource_path}/")))
}

/// The issuers a client would expect: the ones the resource metadata names, or
/// failing that the one the fetched metadata URL was derived from.
fn expected_issuers(report: &Report) -> Vec<String> {
    let named = report
        .protected_resource
        .as_ref()
        .map(|found| found.authorization_servers.clone())
        .unwrap_or_default();
    if !named.is_empty() {
        return named;
    }
    report
        .authorization_server
        .iter()
        .map(|auth| {
            auth.metadata_url
                .replace("/.well-known/oauth-authorization-server", "")
                .replace("/.well-known/openid-configuration", "")
        })
        .collect()
}

/// Issuers compare as strings, except for a trailing slash: strict clients do
/// refuse one, but so many deployments differ only by it that flagging it would
/// bury the mismatches that break everyone.
fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Both well-known spellings for a resource, most specific first.
fn well_known_locations(endpoint: &url::Url, document: &str) -> Vec<String> {
    let mut origin = endpoint.clone();
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    let origin = origin.to_string();
    let origin = origin.trim_end_matches('/');

    let path = endpoint.path().trim_end_matches('/');
    let mut locations = Vec::new();
    if !path.is_empty() && path != "/" {
        locations.push(format!("{origin}/.well-known/{document}{path}"));
    }
    locations.push(format!("{origin}/.well-known/{document}"));
    locations
}

/// `Bearer resource_metadata="https://…"` — RFC 9728's pointer home.
pub(crate) fn resource_metadata_from_challenge(challenge: &str) -> Option<String> {
    let start = challenge.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &challenge[start..];
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find(['"', ',']).unwrap_or(rest.len());
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn fetch_protected_resource(
    client: &httpguard::Client,
    url: &str,
) -> Option<ProtectedResource> {
    let json = fetch_json(client, url).await?;
    Some(ProtectedResource {
        metadata_url: url.to_string(),
        resource: string_at(&json, "resource"),
        authorization_servers: string_list(&json, "authorization_servers"),
        scopes: string_list(&json, "scopes_supported"),
        documentation: string_at(&json, "resource_documentation"),
    })
}

async fn fetch_authorization_server(client: &httpguard::Client, url: &str) -> Option<AuthServer> {
    let json = fetch_json(client, url).await?;
    // Without an issuer this is not RFC 8414 metadata, whatever else it is.
    let issuer = string_at(&json, "issuer")?;
    Some(AuthServer {
        metadata_url: url.to_string(),
        issuer,
        authorization_endpoint: string_at(&json, "authorization_endpoint"),
        token_endpoint: string_at(&json, "token_endpoint"),
        registration_endpoint: string_at(&json, "registration_endpoint"),
        client_id_metadata_document: json
            .get("client_id_metadata_document_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        code_challenge_methods: string_list(&json, "code_challenge_methods_supported"),
        grant_types: string_list(&json, "grant_types_supported"),
        scopes: string_list(&json, "scopes_supported"),
    })
}

async fn fetch_json(client: &httpguard::Client, url: &str) -> Option<Value> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
