//! Translating a stored server row into something that can be read.
//!
//! The store keeps transport configuration as opaque JSON so it never has to
//! know how MCP works. This module is the one place that knows both shapes —
//! a transport `mcpclient` can dial, or, for a WebMCP page, a scan `webprobe`
//! can run.

use std::collections::BTreeMap;
use std::path::PathBuf;

use mcpclient::Transport;
use mcpstore::{ServerId, TransportKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StdioConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpConfig {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Who a scan is, as far as the page is concerned.
///
/// A page shows a stranger one set of tools and a signed-in user another, and
/// **both are real contracts**. The bug would be comparing them: diff a
/// signed-in baseline against an anonymous scan and every members-only tool
/// reads as removed — a breaking verdict for a page that never changed.
///
/// So this is part of a row's identity, not a note attached to a snapshot. Two
/// principals of one page are two rows with two histories, and a cross-
/// principal comparison is simply not something the app can express.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Principal {
    /// A throwaway profile: whatever the page shows a stranger. The only shape
    /// CI can reproduce, so it is also what `mcpi-cli` records.
    #[default]
    Anonymous,
    /// mcpi's own browser profile, which somebody has signed in.
    SignedIn,
}

impl Principal {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::SignedIn => "signed in",
        }
    }
}

/// A WebMCP page: a URL, and who to be when reading it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebMcpConfig {
    pub url: String,
    #[serde(default)]
    pub principal: Principal,
}

/// The keychain account holding a server's OAuth credentials.
///
/// Keyed by row id rather than by URL: two saved servers can point at the same
/// deployment and still have been authorized as different users.
pub fn credential_key(server_id: ServerId) -> String {
    format!("server-{server_id}")
}

/// How to dial a row, or `None` when it is not dialled at all.
///
/// A WebMCP page is read in a browser rather than connected to, so it has no
/// `Transport`. Returning `None` keeps that a case the caller has to handle
/// instead of a transport that would be wrong in some quiet way.
pub fn to_transport(
    kind: TransportKind,
    config: &Value,
    server_id: ServerId,
) -> Result<Option<Transport>, serde_json::Error> {
    Ok(Some(match kind {
        TransportKind::Stdio => {
            let c: StdioConfig = serde_json::from_value(config.clone())?;
            Transport::Stdio {
                command: c.command,
                args: c.args,
                env: c.env,
                cwd: c.cwd.map(PathBuf::from),
            }
        }
        TransportKind::Http => {
            let c: HttpConfig = serde_json::from_value(config.clone())?;
            Transport::Http {
                url: c.url,
                headers: c.headers,
                credential_key: Some(credential_key(server_id)),
            }
        }
        TransportKind::WebMcp => return Ok(None),
    }))
}

/// The origin a row points at, or `None` when it does not point at one.
///
/// A stdio row is a local process, not a surface of a site — grouping one with
/// a web page would be a guess, so it never offers an origin to group on.
pub fn origin_of(kind: TransportKind, config: &Value) -> Option<String> {
    match kind {
        TransportKind::Stdio => None,
        TransportKind::Http | TransportKind::WebMcp => origin(config.get("url")?.as_str()?),
    }
}

/// The origin of a URL as typed, for comparing two of them.
pub fn origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    // `Url` yields "null" for opaque origins (data:, file:), which would then
    // group every such row with every other. Only a real host can group.
    parsed
        .has_host()
        .then(|| parsed.origin().ascii_serialization())
}

/// Where mcpi keeps the browser profile it owns.
///
/// Beside the store rather than in Chrome's own directory: Chrome refuses a
/// debugging port on its default profile (since 136), and copying a real
/// profile is both fragile and what credential stealers do. This one is ours,
/// and the only thing in it is whatever the user signed into through mcpi.
pub fn browser_profile() -> PathBuf {
    mcpstore::default_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("chrome")
}

/// Whether a row is a signed-in page — the only kind the admission gate
/// applies to.
///
/// A free function rather than a method on the app so the predicate can be
/// tested on its own: it is the switch the whole gate hangs off, and getting
/// it backwards would disable the gate silently rather than loudly.
pub fn is_signed_in_page(kind: TransportKind, config: &Value) -> bool {
    kind == TransportKind::WebMcp
        && serde_json::from_value::<WebMcpConfig>(config.clone())
            .is_ok_and(|c| c.principal == Principal::SignedIn)
}

/// The scan a WebMCP row describes.
///
/// The principal picks the profile, and the profile is the whole of what a
/// scan can see — an anonymous row is read in a throwaway directory that is
/// signed out by construction, so it cannot accidentally pick up a session.
pub fn to_scan(config: &Value) -> Result<webprobe::Scan, serde_json::Error> {
    let c: WebMcpConfig = serde_json::from_value(config.clone())?;
    let browser = match c.principal {
        Principal::Anonymous => webprobe::Browser::anonymous(),
        Principal::SignedIn => webprobe::Browser::profile(browser_profile()),
    };
    Ok(webprobe::Scan::new(c.url, browser))
}

/// One entry per line, blanks dropped.
///
/// Arguments are line-separated rather than space-separated so a path with a
/// space in it does not silently split into two arguments.
pub fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// `KEY=value` (or `Key: value`) per line, blanks and unparseable lines dropped.
pub fn pairs(text: &str, separator: char) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(separator)?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Render a map back into the textarea form used by [`pairs`].
pub fn unpairs(map: &BTreeMap<String, String>, separator: &str) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}{separator}{v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_drops_blanks_and_trims() {
        assert_eq!(lines("  a \n\n b\n"), ["a", "b"]);
        assert_eq!(lines(""), Vec::<String>::new());
    }

    #[test]
    fn an_argument_containing_a_space_stays_one_argument() {
        assert_eq!(
            lines("--config\n/Users/me/My Documents/cfg.json"),
            ["--config", "/Users/me/My Documents/cfg.json"]
        );
    }

    #[test]
    fn pairs_splits_on_the_first_separator_only() {
        // Values routinely contain the separator — a bearer token has colons,
        // a connection string has equals signs.
        let parsed = pairs("Authorization: Bearer a:b:c\nX-Trace: 1", ':');
        assert_eq!(parsed["Authorization"], "Bearer a:b:c");
        assert_eq!(parsed["X-Trace"], "1");
    }

    #[test]
    fn pairs_ignores_lines_without_a_separator() {
        let parsed = pairs("GOOD=1\nnonsense\n=novalue", '=');
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed["GOOD"], "1");
    }

    #[test]
    fn pairs_and_unpairs_round_trip() {
        let text = "A=1\nB=2";
        assert_eq!(unpairs(&pairs(text, '='), "="), text);
    }

    #[test]
    fn stdio_config_survives_the_trip_through_json() {
        let config = serde_json::json!({
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-everything"],
        });
        let transport = to_transport(TransportKind::Stdio, &config, 1)
            .unwrap()
            .unwrap();
        match transport {
            Transport::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn a_config_missing_optional_fields_still_loads() {
        // Rows written by an older build must not become unopenable.
        let transport = to_transport(TransportKind::Http, &serde_json::json!({ "url": "u" }), 1);
        assert!(transport.is_ok());
    }

    #[test]
    fn a_page_has_no_transport() {
        // Not an error — a page is simply not something that gets dialled.
        let config = serde_json::json!({ "url": "https://app.example.com" });
        assert!(
            to_transport(TransportKind::WebMcp, &config, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_page_without_a_principal_is_anonymous() {
        // Rows written before principals existed, and every row the CLI would
        // recognise. Defaulting the other way would silently promote an old
        // row to a signed-in series it never recorded.
        let config = serde_json::json!({ "url": "https://app.example.com" });
        let parsed: WebMcpConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!(parsed.principal, Principal::Anonymous);

        let scan = to_scan(&config).unwrap();
        assert!(matches!(
            scan.browser,
            webprobe::Browser::Launched { profile: None, .. }
        ));
    }

    #[test]
    fn only_a_signed_in_page_is_gated() {
        let page = |principal: &str| serde_json::json!({ "url": "https://app.example.com", "principal": principal });

        assert!(is_signed_in_page(TransportKind::WebMcp, &page("signed_in")));
        // An anonymous series has no session to lose, so gating it would only
        // ever refuse scans for no reason.
        assert!(!is_signed_in_page(
            TransportKind::WebMcp,
            &page("anonymous")
        ));
        // A dialled server never reaches the gate at all.
        assert!(!is_signed_in_page(
            TransportKind::Http,
            &serde_json::json!({ "url": "https://app.example.com/mcp" })
        ));
        // A row written before principals existed is anonymous, not gated.
        assert!(!is_signed_in_page(
            TransportKind::WebMcp,
            &serde_json::json!({ "url": "https://app.example.com" })
        ));
    }

    #[test]
    fn a_signed_in_page_reads_the_profile_mcpi_owns() {
        let config = serde_json::json!({
            "url": "https://app.example.com",
            "principal": "signed_in",
        });

        let scan = to_scan(&config).unwrap();
        let webprobe::Browser::Launched {
            profile, headless, ..
        } = scan.browser
        else {
            panic!("a page is never dialled");
        };
        assert_eq!(profile, Some(browser_profile()));
        // Rescans must not throw a window up; signing in is its own action.
        assert!(headless);
    }
}
