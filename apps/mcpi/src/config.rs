//! Translating a stored server row into something `mcpclient` can dial.
//!
//! The store keeps transport configuration as opaque JSON so it never has to
//! know how MCP works. This module is the one place that knows both shapes.

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

/// The keychain account holding a server's OAuth credentials.
///
/// Keyed by row id rather than by URL: two saved servers can point at the same
/// deployment and still have been authorized as different users.
pub fn credential_key(server_id: ServerId) -> String {
    format!("server-{server_id}")
}

pub fn to_transport(
    kind: TransportKind,
    config: &Value,
    server_id: ServerId,
) -> Result<Transport, serde_json::Error> {
    Ok(match kind {
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
    })
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
        let transport = to_transport(TransportKind::Stdio, &config, 1).unwrap();
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
}
