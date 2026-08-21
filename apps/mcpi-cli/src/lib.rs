//! The CI half of mcpi: snapshot a server's contract, diff two contracts,
//! exit non-zero when the change would break callers.
//!
//! Everything judgemental is borrowed — `schemadiff` classifies, `mcpclient`
//! connects, `mcpstore` holds the app's named baselines. This crate only
//! decides what a source string means and what the exit code says:
//!
//! - `0` — no changes, or nothing breaking
//! - `1` — at least one breaking change
//! - `2` — the comparison itself failed
//!
//! A pipeline can therefore gate on `mcpi-cli diff` directly, and `1` is never
//! ambiguous between "your contract broke" and "the tool fell over".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mcpclient::{Handle, Transport};
use mcpstore::Store;
use schemadiff::{Snapshot, SnapshotDiff};

/// One side of a comparison, as typed on the command line.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// A snapshot file written by `mcpi-cli snapshot`.
    File(PathBuf),
    /// A live remote server, snapshotted on the spot.
    Http(String),
    /// A local server spawned over stdio, snapshotted on the spot.
    Stdio { command: String, args: Vec<String> },
    /// A baseline pinned in the desktop app (`@v1.2`).
    Baseline(String),
}

/// What a source string means. Pure — nothing is read or dialled here.
///
/// `@name` is a baseline, `stdio:` spawns, `http(s)://` dials, and anything
/// else is a file path — a missing file fails at read time with all four
/// forms named, so a typo gets the whole menu rather than "not found".
pub fn parse_source(raw: &str) -> Result<Source, String> {
    if let Some(label) = raw.strip_prefix('@') {
        if label.is_empty() {
            return Err("`@` names a baseline, but the name is missing (try `@v1.2`)".into());
        }
        return Ok(Source::Baseline(label.to_string()));
    }
    if let Some(rest) = raw.strip_prefix("stdio:") {
        // Split on whitespace, same as a shell would to a first approximation.
        // An argument with spaces in it cannot be expressed; CI commands do
        // not need it, and the desktop app remains the place for exotic setups.
        let mut parts = rest.split_whitespace().map(str::to_string);
        let Some(command) = parts.next() else {
            return Err(
                "`stdio:` needs a command to spawn (try `stdio:npx -y some-server`)".into(),
            );
        };
        return Ok(Source::Stdio {
            command,
            args: parts.collect(),
        });
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok(Source::Http(raw.to_string()));
    }
    Ok(Source::File(PathBuf::from(raw)))
}

/// `"Name: value"` pairs from repeated `--header` flags.
pub fn parse_headers(raw: &[String]) -> Result<BTreeMap<String, String>, String> {
    raw.iter()
        .map(|h| {
            let (name, value) = h
                .split_once(':')
                .ok_or_else(|| format!("header `{h}` is not in `Name: value` form"))?;
            Ok((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Resolve a source to a snapshot plus a short description for the report
/// heading.
pub async fn resolve(
    source: &Source,
    headers: &BTreeMap<String, String>,
    store_path: &Path,
    server: Option<&str>,
) -> Result<(Snapshot, String), String> {
    match source {
        Source::File(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                format!(
                    "could not read `{}`: {e}. A source is a snapshot file, an \
                     http(s):// URL, a `stdio:` command, or an `@baseline`.",
                    path.display()
                )
            })?;
            let snapshot: Snapshot = serde_json::from_str(&text)
                .map_err(|e| format!("`{}` is not a snapshot file: {e}", path.display()))?;
            Ok((snapshot, path.display().to_string()))
        }
        Source::Http(url) => {
            let transport = Transport::Http {
                url: url.clone(),
                headers: headers.clone(),
                // No keychain from a pipeline; auth comes in via --header.
                credential_key: None,
            };
            Ok((live_snapshot(&transport, url).await?, url.clone()))
        }
        Source::Stdio { command, args } => {
            let transport = Transport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: BTreeMap::new(),
                cwd: None,
            };
            // The full command line, not just the binary: `--variant a` versus
            // `--variant b` is often the entire difference between the sides.
            let description = std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            Ok((live_snapshot(&transport, command).await?, description))
        }
        Source::Baseline(label) => {
            let store = Store::open(store_path).map_err(|e| {
                format!(
                    "could not open the store at `{}`: {e}",
                    store_path.display()
                )
            })?;
            let (snapshot, description) = baseline(&store, label, server)?;
            Ok((snapshot, description))
        }
    }
}

async fn live_snapshot(transport: &Transport, name: &str) -> Result<Snapshot, String> {
    let (handle, _) = Handle::connect(transport).await.map_err(|e| match e {
        mcpclient::Error::AuthRequired { .. } => format!(
            "`{name}` requires sign-in. Pass a token with --header \
             \"Authorization: Bearer …\" — the CLI never opens a browser."
        ),
        other => format!("could not connect to `{name}`: {other}"),
    })?;
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|e| format!("connected to `{name}`, but the snapshot failed: {e}"));
    let _ = handle.shutdown().await;
    snapshot
}

/// Find the one snapshot pinned under `label`.
///
/// Labels are unique per server, not globally, so two servers can both have a
/// `v1` — that case asks for `--server` rather than guessing.
fn baseline(
    store: &Store,
    label: &str,
    server: Option<&str>,
) -> Result<(Snapshot, String), String> {
    let servers = store
        .list_servers()
        .map_err(|e| format!("could not list servers: {e}"))?;

    let mut matches = Vec::new();
    let mut available = Vec::new();
    for row in servers {
        if let Some(wanted) = server
            && row.name != wanted
        {
            continue;
        }
        for pinned in store
            .labeled_snapshots(row.id)
            .map_err(|e| format!("could not read baselines for `{}`: {e}", row.name))?
        {
            let name = pinned.label.clone().unwrap_or_default();
            if name == label {
                matches.push((row.name.clone(), pinned));
            } else {
                available.push(format!("@{name} ({})", row.name));
            }
        }
    }

    match matches.len() {
        1 => {
            let (server_name, pinned) = matches.remove(0);
            let when = pinned.taken_at.format("%-d %b %Y %H:%M");
            Ok((
                pinned.snapshot,
                format!("@{label} · {server_name} · {when}"),
            ))
        }
        0 if available.is_empty() => Err(format!(
            "no baseline named `@{label}`. Pin one in the app's contract history first."
        )),
        0 => Err(format!(
            "no baseline named `@{label}`. Available: {}",
            available.join(", ")
        )),
        _ => Err(format!(
            "`@{label}` is pinned on more than one server ({}). Pass --server to pick one.",
            matches
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Push a snapshot into a listing's public contract history on the directory.
///
/// Owner-gated server-side: the API key's account must hold the listing's
/// verified DNS claim, and the entry is displayed as "owner-reported" — never
/// as probe-verified. Returns whether a row was recorded; `false` means the
/// digest matched the latest snapshot, i.e. nothing moved.
pub async fn publish(
    site: &str,
    listing: &str,
    api_key: &str,
    snapshot: &Snapshot,
) -> Result<bool, String> {
    let url = format!(
        "{}/api/directory/servers/{listing}/snapshot",
        site.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .post(&url)
        .bearer_auth(api_key)
        .json(snapshot)
        .send()
        .await
        .map_err(|e| format!("could not reach `{url}`: {e}"))?;

    match response.status().as_u16() {
        200 => {
            #[derive(serde::Deserialize)]
            struct Pushed {
                recorded: bool,
            }
            let pushed: Pushed = response
                .json()
                .await
                .map_err(|e| format!("unexpected response from `{url}`: {e}"))?;
            Ok(pushed.recorded)
        }
        401 | 403 => Err(
            "the directory rejected the API key — it must belong to the account holding this \
             listing's verified claim"
                .into(),
        ),
        404 => Err(format!(
            "no listing `{listing}` on the directory. The slug is the last part of the \
             listing's URL."
        )),
        other => Err(format!("`{url}` answered {other}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// The classified report, readable in a terminal and pasteable into a PR.
    Markdown,
    /// The raw `SnapshotDiff`, for jq and friends.
    Json,
}

/// Render a diff the way the drawer's export button would.
pub fn render(diff: &SnapshotDiff, subtitle: &str, format: Format) -> String {
    match format {
        Format::Markdown => schemadiff::to_markdown(diff, subtitle),
        Format::Json => serde_json::to_string_pretty(diff).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_form_parses() {
        assert_eq!(
            parse_source("@v1.2").unwrap(),
            Source::Baseline("v1.2".into())
        );
        assert_eq!(
            parse_source("https://api.example.com/mcp").unwrap(),
            Source::Http("https://api.example.com/mcp".into())
        );
        assert_eq!(
            parse_source("stdio:npx -y some-server").unwrap(),
            Source::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "some-server".into()],
            }
        );
        assert_eq!(
            parse_source("snapshots/prod.json").unwrap(),
            Source::File(PathBuf::from("snapshots/prod.json"))
        );
    }

    #[test]
    fn empty_prefixes_are_named_errors() {
        assert!(parse_source("@").is_err());
        assert!(parse_source("stdio:").is_err());
    }

    #[test]
    fn headers_split_on_the_first_colon_only() {
        let parsed = parse_headers(&["Authorization: Bearer a:b:c".into()]).unwrap();
        assert_eq!(parsed["Authorization"], "Bearer a:b:c");
        assert!(parse_headers(&["no-colon".into()]).is_err());
    }
}
