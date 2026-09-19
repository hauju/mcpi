//! `mcpi-cli` — snapshot MCP servers and fail the build on breaking changes.

use std::path::PathBuf;
use std::process::ExitCode;

use std::net::SocketAddr;

use clap::{Parser, Subcommand, ValueEnum};
use mcpi_cli::{Format, parse_headers, parse_source, render, resolve};
use mcprouter::Upstreams as _;
use schemadiff::Severity;

#[derive(Clone, Copy, ValueEnum)]
enum GatewayTransport {
    Stdio,
    Http,
}

#[derive(Parser)]
#[command(name = "mcpi-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// A source is a snapshot file, an `http(s)://` URL (snapshotted live), a
/// `stdio:command args` server (spawned and snapshotted), a
/// `webmcp:https://…` page (read out of a headless Chrome), or an `@baseline`
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
    /// Serve the saved server library as one MCP gateway with two tools,
    /// `find_tools` and `call_tool`, routed by TypeSafe Jev.
    Serve {
        /// Only these saved servers (by name). Default: every dialable one.
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Speak MCP on stdin/stdout, or serve streamable HTTP at `/mcp`.
        #[arg(long, value_enum, default_value = "stdio")]
        transport: GatewayTransport,
        /// Bind address for `--transport http`.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
        /// TypeSafe API key. Also read from `.env` in the working directory.
        #[arg(long, env = "TYPESAFE_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// Tools (gateway names) to list directly, without a find_tools round trip.
        #[arg(long = "expose-direct")]
        expose_direct: Vec<String>,
        /// Default number of tools find_tools returns.
        #[arg(long, default_value_t = 5)]
        k: usize,
        /// BM25 shortlist sent to Jev; 0 sends the whole catalog.
        #[arg(long, default_value_t = 0)]
        shortlist: usize,
        /// JSONL log of routing decisions and calls. Default: `router.jsonl` beside the store.
        #[arg(long)]
        log: Option<PathBuf>,
        /// Do not write the JSONL log.
        #[arg(long)]
        no_log: bool,
        /// The mcpi app's database.
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Summarise the gateway's JSONL log: which tools are used, which the router
    /// missed, and which are worth exposing directly.
    Stats {
        /// The log to read. Default: `router.jsonl` beside the store.
        log: Option<PathBuf>,
        /// The mcpi app's database, to locate the default log.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Recommend `direct` when a tool is called in at least this share of tool-using turns.
        #[arg(long, default_value_t = 0.3)]
        direct_share: f64,
        /// Recommend `direct` when the router missed the tool before at least this share of its calls.
        #[arg(long, default_value_t = 0.1)]
        direct_miss: f64,
        /// Minimum calls before a recommendation is made.
        #[arg(long, default_value_t = 5)]
        min_calls: usize,
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
        Command::Serve {
            servers,
            transport,
            bind,
            api_key,
            expose_direct,
            k,
            shortlist,
            log,
            no_log,
            store,
        } => {
            let store_path = store.unwrap_or_else(mcpstore::default_path);
            let store = mcpstore::Store::open(&store_path).map_err(|e| {
                format!(
                    "could not open the store at `{}`: {e}",
                    store_path.display()
                )
            })?;
            let api_key = api_key
                .or_else(|| mcpi_cli::serve::dotenv_value(mcprouter::jev::API_KEY_ENV))
                .ok_or(
                    "no TypeSafe API key: pass --api-key, set TYPESAFE_API_KEY, or put it in .env",
                )?;
            let jev = mcprouter::Jev::new(api_key, mcprouter::jev::DEFAULT_MODEL)
                .map_err(|e| e.to_string())?;

            let live = mcpi_cli::serve::Live::connect(&store, &servers).await?;
            let notes = live.notes.clone();
            let settings = mcprouter::RouterSettings {
                k_default: k,
                shortlist,
                expose_direct,
                ..Default::default()
            };
            let router = mcprouter::Router::new(jev, settings, live.entries());
            let log = if no_log {
                None
            } else {
                let path = log.unwrap_or_else(|| mcpi_cli::serve::default_log_path(&store_path));
                Some(
                    mcprouter::log::JsonlLog::open(&path).await.map_err(|e| {
                        format!("could not open the log at `{}`: {e}", path.display())
                    })?,
                )
            };
            for n in &notes {
                eprintln!(
                    "`{}` changed since last seen: {} breaking, {} compatible, {} cosmetic",
                    n.server, n.breaking, n.compatible, n.cosmetic
                );
            }
            let gateway =
                mcprouter::Gateway::new(mcpi_cli::serve::shared(live), router, log, notes);
            gateway.log_catalog().await;
            eprintln!("gateway ready: {} tools", gateway.entries().len());
            match transport {
                GatewayTransport::Stdio => mcprouter::serve::serve_stdio(gateway).await,
                GatewayTransport::Http => mcprouter::serve::serve_http(gateway, bind).await,
            }
            .map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Stats {
            log,
            store,
            direct_share,
            direct_miss,
            min_calls,
        } => {
            let path = log.unwrap_or_else(|| {
                mcpi_cli::serve::default_log_path(&store.unwrap_or_else(mcpstore::default_path))
            });
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read `{}`: {e}", path.display()))?;
            let stats = mcprouter::Stats::from_jsonl(&text);
            print!(
                "{}",
                stats.render(mcprouter::Thresholds {
                    direct_share,
                    direct_miss,
                    min_calls
                })
            );
            Ok(ExitCode::SUCCESS)
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
