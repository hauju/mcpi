//! Finding a site's MCP endpoint from nothing but its domain.
//!
//! One input — `stripe.com`, typed with or without a scheme — and the
//! question "does this product have an MCP server, and where?" The search
//! space is small because deployments are conventional: the endpoint lives at
//! `/mcp` on the main host or on an `mcp.` subdomain, occasionally at `/sse`
//! for the legacy transport, and the pages that miss name the URL they
//! document. Two bounded waves cover all of it — the conventions, then
//! whatever the fetched pages themselves referred to. No crawl, no headless
//! browser: the long tail past these conventions is exactly the part a person
//! can paste as a URL directly.
//!
//! Every candidate goes through the caller's `approve` hook before any
//! request is made. This crate has no opinion about which addresses are safe
//! to fetch — a server doing this on behalf of anonymous users must refuse
//! private and internal addresses, and only the caller knows its network — so
//! approval is policy and stays outside the engine.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::{EndpointKind, Report, endpoint};

/// Per-request budget. Tighter than the single-endpoint probe's: a discovery
/// fans out over candidates that mostly do not exist, a person is usually
/// watching, and a host that never answers should cost seconds, not the full
/// probe timeout multiplied by the candidate count.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// Cap on the homepage body scanned for referrals. Well above any real
/// document; the body is whatever the site chose to serve, so it is read in
/// chunks against this cap rather than trusted to be finite.
const MAX_HTML_BYTES: usize = 512 * 1024;

/// Cap on second-wave candidates. Pages name what they like; this bounds how
/// much fetching a single discovery can be talked into.
const MAX_REFERRALS: usize = 5;

/// Where a candidate URL came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// The URL as the caller entered it, when it pointed at more than a
    /// homepage.
    Entered,
    /// A deployment convention: `/mcp` on the entered host, the `mcp.` or
    /// `api.` subdomain, `/sse` for the legacy transport.
    Convention,
    /// Named by the site itself — found in the homepage markup, or on a page
    /// a convention candidate turned out to be.
    Referred,
}

/// An endpoint that answered the MCP handshake, with the identification that
/// proved it. The report is the light half of a [`crate::probe`] — transport
/// and identity, no OAuth discovery, no CORS check — so run the full probe on
/// the URLs worth keeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovered {
    pub url: String,
    pub source: CandidateSource,
    pub report: Report,
}

/// What a discovery tried and what it found. Empty `found` over a populated
/// `tried` is the answer "no MCP endpoint anywhere anyone would look for
/// one", which is worth as much as a hit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Discovery {
    /// The host the search anchored to, normalized from the input.
    pub host: String,
    /// Every URL actually contacted, in the order contacted.
    pub tried: Vec<String>,
    /// Endpoints that spoke MCP — including auth-gated servers that answer
    /// the handshake with a challenge — or the legacy HTTP+SSE transport,
    /// which is a real server needing a client with a fallback. Aliases
    /// answering with the same server name and version collapse into the
    /// first hit.
    pub found: Vec<Discovered>,
}

/// Discover MCP endpoints for a site.
///
/// Never fails: input that does not parse as an http(s) host returns an empty
/// [`Discovery`]. `approve` is asked about every URL before it is contacted —
/// return `false` to skip one; see the module docs for why that policy lives
/// with the caller.
pub async fn discover(input: &str, approve: impl AsyncFn(&str) -> bool) -> Discovery {
    let Some(base) = base_url(input) else {
        return Discovery::default();
    };
    let mut discovery = Discovery {
        host: base.host_str().unwrap_or_default().to_string(),
        ..Default::default()
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("mcpi/probe")
        .build()
    else {
        return discovery;
    };

    // First wave: the input itself when it points somewhere specific, then
    // the conventions. The homepage is fetched alongside — with GET, because
    // the wave's initialize POST at a site root is answered by routers and
    // CDNs with an error page, not the document that names the endpoint.
    let mut first: Vec<(String, CandidateSource)> = Vec::new();
    if base.path() != "/" || base.query().is_some() {
        push_unique(&mut first, base.to_string(), CandidateSource::Entered);
    }
    for url in convention_urls(&base) {
        push_unique(&mut first, url, CandidateSource::Convention);
    }
    let mut approved_first = Vec::new();
    for (url, source) in first {
        if approve(&url).await {
            approved_first.push((url, source));
        }
    }

    let homepage = homepage_url(&base);
    let fetch_homepage = approve(&homepage).await;

    let (first, referrals) = tokio::join!(identify_all(&client, approved_first), async {
        if fetch_homepage {
            homepage_referrals(&client, &homepage).await
        } else {
            Vec::new()
        }
    });
    discovery
        .tried
        .extend(first.iter().map(|candidate| candidate.url.clone()));
    if fetch_homepage {
        discovery.tried.push(homepage);
    }

    // Second wave: what the site itself named — in its homepage, or on pages
    // the first wave landed on. One level only; a referral's referrals are
    // where discovery stops and a pasted URL takes over.
    let mut second: Vec<(String, CandidateSource)> = Vec::new();
    for url in referrals {
        push_unique(&mut second, url, CandidateSource::Referred);
    }
    for candidate in &first {
        if let Some(EndpointKind::WebPage { candidates }) = &candidate.report.kind {
            for url in candidates {
                push_unique(&mut second, url.clone(), CandidateSource::Referred);
            }
        }
    }
    second.retain(|(url, _)| !discovery.tried.contains(url));
    second.truncate(MAX_REFERRALS);
    let mut approved_second = Vec::new();
    for (url, source) in second {
        if approve(&url).await {
            approved_second.push((url, source));
        }
    }
    let second = identify_all(&client, approved_second).await;
    discovery
        .tried
        .extend(second.iter().map(|candidate| candidate.url.clone()));

    discovery.found = collapse_aliases(first.into_iter().chain(second).filter(spoke_mcp).collect());
    discovery
}

/// Accept what people type: a bare domain, or a full URL pasted from
/// documentation. Anything that does not come out as an http(s) host is a
/// `None`, not a guess.
fn base_url(raw: &str) -> Option<url::Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = url::Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()?;
    Some(url)
}

/// The deployment conventions, most likely first. Subdomain variants exist
/// only for named hosts — deriving `mcp.` from an IP address would invent a
/// DNS name — and anchor to the host minus any leading `www.`, which is where
/// the conventions live on real sites.
pub(crate) fn convention_urls(base: &url::Url) -> Vec<String> {
    let mut root = base.clone();
    root.set_path("/");
    root.set_query(None);
    root.set_fragment(None);

    let mut urls = Vec::new();
    if let Ok(joined) = root.join("/mcp") {
        urls.push(joined.to_string());
    }
    if let Some(url::Host::Domain(domain)) = root.host() {
        let apex = domain.strip_prefix("www.").unwrap_or(domain).to_string();
        let derive = |sub: &str, path: &str| -> Option<String> {
            if apex.starts_with(&format!("{sub}.")) {
                return None;
            }
            let mut derived = root.clone();
            derived.set_host(Some(&format!("{sub}.{apex}"))).ok()?;
            derived.set_path(path);
            Some(derived.to_string())
        };
        urls.extend(derive("mcp", "/mcp"));
        urls.extend(derive("mcp", "/"));
        urls.extend(derive("mcp", "/sse"));
        urls.extend(derive("api", "/mcp"));
    }
    if let Ok(joined) = root.join("/sse") {
        urls.push(joined.to_string());
    }
    urls
}

fn homepage_url(base: &url::Url) -> String {
    let mut root = base.clone();
    root.set_path("/");
    root.set_query(None);
    root.set_fragment(None);
    root.to_string()
}

fn push_unique(list: &mut Vec<(String, CandidateSource)>, url: String, source: CandidateSource) {
    if !list.iter().any(|(existing, _)| *existing == url) {
        list.push((url, source));
    }
}

/// Identify every candidate concurrently, results in the given order. Each
/// gets the light half of a probe: the initialize POST and what follows from
/// it, nothing more.
async fn identify_all(
    client: &reqwest::Client,
    candidates: Vec<(String, CandidateSource)>,
) -> Vec<Discovered> {
    let mut set = JoinSet::new();
    for (index, (url, source)) in candidates.into_iter().enumerate() {
        let client = client.clone();
        set.spawn(async move {
            let mut report = Report {
                url: url.clone(),
                ..Default::default()
            };
            endpoint::identify(&client, &url, &mut report).await;
            (
                index,
                Discovered {
                    url,
                    source,
                    report,
                },
            )
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(result) = joined {
            results.push(result);
        }
    }
    results.sort_by_key(|(index, _)| *index);
    results
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
}

/// GET the homepage and pull out the MCP-looking URLs it names.
async fn homepage_referrals(client: &reqwest::Client, url: &str) -> Vec<String> {
    let Ok(mut response) = client.get(url).send().await else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= MAX_HTML_BYTES {
            break;
        }
    }
    endpoint::mcp_candidates(&String::from_utf8_lossy(&bytes), url)
}

fn spoke_mcp(candidate: &Discovered) -> bool {
    matches!(
        candidate.report.kind,
        Some(EndpointKind::Mcp | EndpointKind::LegacySse)
    )
}

/// Collapse aliases: the same server is commonly reachable at more than one
/// convention (`/mcp` on the apex and on the `mcp.` subdomain), and reporting
/// both would double-count it. Identity is the server's own name and version;
/// hits without one — auth-gated servers answer the handshake without
/// introducing themselves — are all kept, because nothing proves them equal.
fn collapse_aliases(hits: Vec<Discovered>) -> Vec<Discovered> {
    let mut seen: Vec<(String, String)> = Vec::new();
    hits.into_iter()
        .filter(|hit| {
            let Some(server) = &hit.report.server else {
                return true;
            };
            if server.name.is_empty() {
                return true;
            }
            let key = (server.name.clone(), server.version.clone());
            if seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        })
        .collect()
}
