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

use crate::Report;

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

pub(crate) async fn resolve(client: &reqwest::Client, url: &str, report: &mut Report) {
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
    client: &reqwest::Client,
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

async fn fetch_authorization_server(client: &reqwest::Client, url: &str) -> Option<AuthServer> {
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

async fn fetch_json(client: &reqwest::Client, url: &str) -> Option<Value> {
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
