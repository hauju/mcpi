//! Reading servers out of a config file someone already wrote.
//!
//! Nobody configures an MCP server twice on purpose. The entry that already
//! works in Claude Desktop or Cursor is the one worth inspecting, and retyping
//! it is both a tax and a source of "it works there but not here" confusion
//! that is really a typo.
//!
//! Every client writes the same two shapes with a different key, so this parses
//! the shapes rather than detecting the vendor: an object of named entries
//! under `mcpServers` (Claude Desktop, Claude Code, Cursor, Cline, Windsurf) or
//! `servers` (VS Code), where each entry is either a command to spawn or a URL
//! to dial. A registry `server.json` is a third shape, handled separately.
//!
//! Placeholders are copied through verbatim. `${input:api-key}` means nothing
//! here, but rewriting it to an empty string would silently produce a server
//! that fails to authenticate for a reason nothing on screen explains; left
//! intact it is visibly a thing to fill in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mcpstore::TransportKind;
use serde_json::Value;

use crate::config::{HttpConfig, StdioConfig};

/// One server found in a config file.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub name: String,
    pub kind: TransportKind,
    /// Already in the shape the store persists.
    pub config: Value,
    /// Why this entry may not work as imported — a placeholder left in place,
    /// a transport we cannot dial. Shown next to the row, never fixed silently.
    pub caveat: Option<String>,
}

/// A config file belonging to another client that exists on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub label: &'static str,
    pub path: PathBuf,
}

/// Where the clients people actually run keep their config, on this platform.
///
/// Only paths that exist are returned, so the import dialog offers real files
/// rather than a checklist of software the user does not have.
pub fn sources() -> Vec<Source> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let candidates: &[(&'static str, &str)] = if cfg!(target_os = "macos") {
        &[
            (
                "Claude Desktop",
                "Library/Application Support/Claude/claude_desktop_config.json",
            ),
            ("Claude Code", ".claude.json"),
            ("Cursor", ".cursor/mcp.json"),
            ("VS Code", "Library/Application Support/Code/User/mcp.json"),
            (
                "Cline",
                "Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json",
            ),
            ("Windsurf", ".codeium/windsurf/mcp_config.json"),
        ]
    } else {
        &[
            (
                "Claude Desktop",
                ".config/Claude/claude_desktop_config.json",
            ),
            ("Claude Code", ".claude.json"),
            ("Cursor", ".cursor/mcp.json"),
            ("VS Code", ".config/Code/User/mcp.json"),
            ("Windsurf", ".codeium/windsurf/mcp_config.json"),
        ]
    };

    candidates
        .iter()
        .map(|(label, relative)| Source {
            label,
            path: home.join(relative),
        })
        .filter(|source| source.path.is_file())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn read(path: &Path) -> Result<Vec<Imported>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text)
}

/// Pull every server out of a client config or a registry `server.json`.
pub fn parse(text: &str) -> Result<Vec<Imported>, String> {
    let root: Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("That is not valid JSON: {e}"))?;

    // A registry entry names itself at the top level and lists no `mcpServers`.
    if root.get("mcpServers").is_none()
        && root.get("servers").is_none()
        && root.get("name").is_some()
    {
        return registry_entry(&root);
    }

    let entries = root
        .get("mcpServers")
        .or_else(|| root.get("servers"))
        // VS Code settings.json nests the same object under `mcp`.
        .or_else(|| root.get("mcp").and_then(|m| m.get("servers")))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "No servers in there. Expected an \"mcpServers\" or \"servers\" object.".to_string()
        })?;

    let mut found: Vec<Imported> = entries
        .iter()
        .filter_map(|(name, entry)| entry_to_server(name, entry))
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));

    if found.is_empty() {
        return Err("Found the servers block, but no entry in it could be read.".into());
    }
    Ok(found)
}

fn entry_to_server(name: &str, entry: &Value) -> Option<Imported> {
    let declared = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // A URL is decisive whatever the entry calls itself; several clients omit
    // `type` entirely and one writes "streamable-http".
    if let Some(url) = entry.get("url").and_then(Value::as_str) {
        let caveat = if declared == "sse" || looks_like_sse(url) {
            // rmcp 3.x ships no legacy SSE client transport, so this will not
            // connect. Importing it anyway beats dropping it without a word.
            Some("Legacy SSE transport — mcpi cannot connect to this".into())
        } else {
            placeholder_caveat(&[url])
        };
        let config = HttpConfig {
            url: url.to_string(),
            headers: string_map(entry.get("headers")),
        };
        return Some(Imported {
            name: name.to_string(),
            kind: TransportKind::Http,
            config: serde_json::to_value(config).ok()?,
            caveat,
        });
    }

    let command = entry.get("command").and_then(Value::as_str)?;
    let args: Vec<String> = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(as_scalar_string).collect())
        .unwrap_or_default();
    let env = string_map(entry.get("env"));

    let mut probes: Vec<&str> = vec![command];
    probes.extend(args.iter().map(String::as_str));
    probes.extend(env.values().map(String::as_str));
    let caveat = placeholder_caveat(&probes);

    let config = StdioConfig {
        command: command.to_string(),
        args,
        env,
        cwd: entry
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|c| !c.is_empty()),
    };
    Some(Imported {
        name: name.to_string(),
        kind: TransportKind::Stdio,
        config: serde_json::to_value(config).ok()?,
        caveat,
    })
}

/// A registry `server.json`, which describes one server rather than a library.
///
/// Only the two forms that map onto a transport we can dial are read: a remote
/// endpoint, and an npm/PyPI package that a runner can execute. A package with
/// a bespoke install step is not something this app can turn into a command,
/// and guessing one would produce a server that fails to spawn.
fn registry_entry(root: &Value) -> Result<Vec<Imported>, String> {
    let name = root
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Imported server");
    // Registry names are reverse-DNS (`io.github.owner/thing`); the last
    // segment is what a person calls it.
    let short = name.rsplit('/').next().unwrap_or(name);

    if let Some(remote) = root
        .get("remotes")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        && let Some(url) = remote.get("url").and_then(Value::as_str)
    {
        let config = HttpConfig {
            url: url.to_string(),
            headers: BTreeMap::new(),
        };
        return Ok(vec![Imported {
            name: short.to_string(),
            kind: TransportKind::Http,
            config: serde_json::to_value(config).map_err(|e| e.to_string())?,
            caveat: None,
        }]);
    }

    if let Some(package) = root
        .get("packages")
        .and_then(Value::as_array)
        .and_then(|p| p.first())
    {
        let identifier = package
            .get("identifier")
            .and_then(Value::as_str)
            .ok_or_else(|| "That registry entry names no package.".to_string())?;
        let registry = package
            .get("registryType")
            .and_then(Value::as_str)
            .unwrap_or("npm");
        let (command, args) = match registry {
            "pypi" => ("uvx", vec![identifier.to_string()]),
            _ => ("npx", vec!["-y".to_string(), identifier.to_string()]),
        };
        let config = StdioConfig {
            command: command.to_string(),
            args,
            env: BTreeMap::new(),
            cwd: None,
        };
        return Ok(vec![Imported {
            name: short.to_string(),
            kind: TransportKind::Stdio,
            config: serde_json::to_value(config).map_err(|e| e.to_string())?,
            caveat: Some("Arguments from the registry entry were not read — check them".into()),
        }]);
    }

    Err("That registry entry describes no remote or package this app can run.".into())
}

/// `${input:…}` (VS Code) and `${env:…}` survive the import so they are visible
/// and editable rather than silently connecting with a literal placeholder.
fn placeholder_caveat(fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .any(|f| f.contains("${"))
        .then(|| "Contains a ${…} placeholder to fill in".into())
}

fn looks_like_sse(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .ends_with("/sse")
}

/// Numbers and booleans appear in `env` blocks in the wild; JSON-encoding them
/// is closer to what the writer meant than dropping the variable.
fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.clone(), as_scalar_string(v)?)))
                .collect()
        })
        .unwrap_or_default()
}

fn as_scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_desktop_shape() {
        let found = parse(
            r#"{"mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                    "env": {"LOG": "debug"}
                }
            }}"#,
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "filesystem");
        assert_eq!(found[0].kind, TransportKind::Stdio);
        assert_eq!(found[0].config["command"], "npx");
        assert_eq!(found[0].config["args"][2], "/tmp");
        assert_eq!(found[0].config["env"]["LOG"], "debug");
    }

    #[test]
    fn vs_code_uses_servers_and_a_type_field() {
        let found =
            parse(r#"{"servers": {"api": {"type": "http", "url": "https://example.com/mcp"}}}"#)
                .unwrap();
        assert_eq!(found[0].kind, TransportKind::Http);
        assert_eq!(found[0].config["url"], "https://example.com/mcp");
    }

    #[test]
    fn vs_code_settings_json_nests_the_same_block() {
        let found =
            parse(r#"{"mcp": {"servers": {"api": {"url": "https://example.com/mcp"}}}}"#).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_sse_entry_is_imported_but_says_it_will_not_connect() {
        // rmcp 3.x has no legacy SSE client transport. Dropping the entry would
        // read as "your config had four servers, we found three".
        let found = parse(r#"{"mcpServers": {"old": {"url": "https://x.dev/sse"}}}"#).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].caveat.as_deref().unwrap().contains("SSE"));
    }

    #[test]
    fn a_placeholder_is_kept_and_flagged() {
        let found = parse(
            r#"{"servers": {"gh": {"command": "npx", "args": ["-y", "srv"],
                "env": {"TOKEN": "${input:github-token}"}}}}"#,
        )
        .unwrap();
        assert_eq!(found[0].config["env"]["TOKEN"], "${input:github-token}");
        assert!(found[0].caveat.is_some());
    }

    #[test]
    fn headers_come_across_on_a_remote_entry() {
        let found = parse(
            r#"{"mcpServers": {"api": {"url": "https://x.dev/mcp",
                "headers": {"Authorization": "Bearer t"}}}}"#,
        )
        .unwrap();
        assert_eq!(found[0].config["headers"]["Authorization"], "Bearer t");
    }

    #[test]
    fn entries_that_are_neither_command_nor_url_are_skipped_not_fatal() {
        let found =
            parse(r#"{"mcpServers": {"broken": {"note": "todo"}, "ok": {"command": "node"}}}"#)
                .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "ok");
    }

    #[test]
    fn a_registry_remote_entry_becomes_one_http_server() {
        let found = parse(
            r#"{"name": "io.github.acme/weather",
                "remotes": [{"type": "streamable-http", "url": "https://acme.dev/mcp"}]}"#,
        )
        .unwrap();
        assert_eq!(found[0].name, "weather");
        assert_eq!(found[0].config["url"], "https://acme.dev/mcp");
    }

    #[test]
    fn a_registry_package_entry_becomes_a_runner_command() {
        let found = parse(
            r#"{"name": "io.github.acme/git",
                "packages": [{"registryType": "pypi", "identifier": "mcp-server-git"}]}"#,
        )
        .unwrap();
        assert_eq!(found[0].config["command"], "uvx");
        assert_eq!(found[0].config["args"][0], "mcp-server-git");
        assert!(found[0].caveat.is_some());
    }

    #[test]
    fn junk_reports_rather_than_panics() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"unrelated": 1}"#).is_err());
        assert!(parse(r#"{"mcpServers": {}}"#).is_err());
    }
}
