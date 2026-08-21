//! `mcpi-cli` — snapshot MCP servers and fail the build on breaking changes.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mcpi_cli::{Format, parse_headers, parse_source, render, resolve};
use schemadiff::Severity;

#[derive(Parser)]
#[command(name = "mcpi-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// A source is a snapshot file, an `http(s)://` URL (snapshotted live), a
/// `stdio:command args` server (spawned and snapshotted), or an `@baseline`
/// pinned in the mcpi app.
#[derive(Subcommand)]
enum Command {
    /// Snapshot a server's contract as JSON, for committing next to the code.
    Snapshot {
        /// What to snapshot.
        source: String,
        /// Extra HTTP header, `"Name: value"`. Repeatable.
        #[arg(long = "header")]
        headers: Vec<String>,
        /// Write here instead of stdout.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// The mcpi app's database, for `@baseline` sources.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Which server a `@baseline` belongs to, when the name is ambiguous.
        #[arg(long)]
        server: Option<String>,
    },
    /// Push a snapshot of your own server into its directory listing's
    /// contract history. Needs the listing's verified claim on your account.
    Publish {
        /// What to snapshot.
        source: String,
        /// The listing's slug on the directory (mcpi.app/servers/<slug>).
        #[arg(long)]
        listing: String,
        /// An API key from the mcpi site (Settings → API keys).
        #[arg(long, env = "MCPI_API_KEY", hide_env_values = true)]
        api_key: String,
        /// The directory to push to.
        #[arg(long, default_value = "https://mcpi.app")]
        site: String,
        /// Extra HTTP header for the snapshot connection, `"Name: value"`.
        #[arg(long = "header")]
        headers: Vec<String>,
        /// The mcpi app's database, for `@baseline` sources.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Which server a `@baseline` belongs to, when the name is ambiguous.
        #[arg(long)]
        server: Option<String>,
    },
    /// Check a contract against the spec's static tool rules. Exits 1 when a
    /// MUST is violated; notes alone exit 0.
    Lint {
        /// What to lint.
        source: String,
        /// Extra HTTP header, `"Name: value"`. Repeatable.
        #[arg(long = "header")]
        headers: Vec<String>,
        /// The mcpi app's database, for `@baseline` sources.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Which server a `@baseline` belongs to, when the name is ambiguous.
        #[arg(long)]
        server: Option<String>,
    },
    /// Classify what changed between two contracts. Exits 1 on breaking.
    Diff {
        /// The older side.
        before: String,
        /// The newer side.
        after: String,
        /// Extra HTTP header, `"Name: value"`. Repeatable.
        #[arg(long = "header")]
        headers: Vec<String>,
        #[arg(long, value_enum, default_value = "markdown")]
        format: Format,
        /// The mcpi app's database, for `@baseline` sources.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Which server a `@baseline` belongs to, when the name is ambiguous.
        #[arg(long)]
        server: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Command::Snapshot {
            source,
            headers,
            out,
            store,
            server,
        } => {
            let source = parse_source(&source)?;
            let headers = parse_headers(&headers)?;
            let store = store.unwrap_or_else(mcpstore::default_path);
            let (snapshot, _) = resolve(&source, &headers, &store, server.as_deref()).await?;

            let json = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| format!("could not serialize the snapshot: {e}"))?;
            match out {
                Some(path) => std::fs::write(&path, json)
                    .map_err(|e| format!("could not write `{}`: {e}", path.display()))?,
                None => println!("{json}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Publish {
            source,
            listing,
            api_key,
            site,
            headers,
            store,
            server,
        } => {
            let source = parse_source(&source)?;
            let headers = parse_headers(&headers)?;
            let store = store.unwrap_or_else(mcpstore::default_path);
            let (snapshot, description) =
                resolve(&source, &headers, &store, server.as_deref()).await?;

            let recorded = mcpi_cli::publish(&site, &listing, &api_key, &snapshot).await?;
            if recorded {
                println!("recorded: {description} → {site}/servers/{listing} (owner-reported)");
            } else {
                println!("unchanged: the listing's latest snapshot already matches");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Lint {
            source,
            headers,
            store,
            server,
        } => {
            let source = parse_source(&source)?;
            let headers = parse_headers(&headers)?;
            let store = store.unwrap_or_else(mcpstore::default_path);
            let (snapshot, description) =
                resolve(&source, &headers, &store, server.as_deref()).await?;

            let findings = mcplint::lint(&snapshot);
            let violated = findings.iter().any(|f| f.level == mcplint::Level::Warning);
            println!("{}", mcplint::to_text(&description, &findings));

            Ok(if violated {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Diff {
            before,
            after,
            headers,
            format,
            store,
            server,
        } => {
            let before = parse_source(&before)?;
            let after = parse_source(&after)?;
            let headers = parse_headers(&headers)?;
            let store = store.unwrap_or_else(mcpstore::default_path);

            let (before, before_desc) =
                resolve(&before, &headers, &store, server.as_deref()).await?;
            let (after, after_desc) = resolve(&after, &headers, &store, server.as_deref()).await?;

            let diff = schemadiff::diff(&before, &after);
            let subtitle = format!("{before_desc} → {after_desc}");
            println!("{}", render(&diff, &subtitle, format));

            Ok(if diff.severity() == Some(Severity::Breaking) {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}
