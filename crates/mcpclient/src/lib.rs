//! An MCP client shaped for a GUI.
//!
//! `rmcp`'s [`RunningService`](rmcp::service::RunningService) is not `Clone` and
//! owns a background task, which makes it awkward to hold in reactive UI state.
//! This crate hides it behind [`Handle`] — a cheap, cloneable, `Send + Sync`
//! command sender — so a UI event handler is just
//! `spawn(async move { handle.call_tool(..).await })` with no lifetime or
//! ownership puzzles, and nothing above this layer needs to know `rmcp` exists
//! beyond its model types.
//!
//! Two things worth knowing:
//!
//! - Commands are dispatched concurrently onto a cloned peer, so a slow tool
//!   call does not block a `tools/list` refresh behind it.
//! - Server-sent notifications (logging, progress, list-changed) are fanned out
//!   on a broadcast channel from the moment a session connects, whether or not
//!   anything is listening yet.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{
    CallToolResult, GetPromptResult, JsonObject, Prompt, ReadResourceResult, Resource,
    ServerPeerInfo, Tool,
};
use tokio::sync::{broadcast, mpsc, oneshot};

mod oauth;
mod pathenv;
mod session;
mod snapshot;
mod wire;

pub use oauth::{sign_in, sign_out};
pub use pathenv::{login_shell_path, resolve_command};
pub use schemadiff::Snapshot;
pub use session::Event;

/// Capacity of the per-session event channel. Events are diagnostic; a
/// listener that falls this far behind is better off being told it lagged than
/// being allowed to pin unbounded memory.
///
/// Sized for the transcript rather than for notifications: every JSON-RPC
/// frame and every line of a chatty server's `stderr` rides this channel, so a
/// single `tools/list` against a large server can spend a dozen slots before
/// anything a person did shows up.
const EVENT_CHANNEL_CAPACITY: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`{command}` was not found. Searched: {searched}")]
    CommandNotFound { command: String, searched: String },

    #[error("failed to start `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "this server speaks the deprecated HTTP+SSE transport (the `/sse` + `POST /messages` pair), \
         which this client does not implement — only stdio and Streamable HTTP are supported"
    )]
    LegacySseTransport,

    #[error("could not connect: {0}")]
    Connect(String),

    #[error("this server requires you to sign in")]
    AuthRequired {
        /// The server's `WWW-Authenticate` header, which tells the OAuth flow
        /// where to discover its authorization server.
        challenge: Option<String>,
    },

    #[error("the server rejected the request: {0}")]
    Request(String),

    #[error("the session has shut down")]
    Closed,
}

type Result<T> = std::result::Result<T, Error>;

/// How to reach a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Spawn a local process and speak MCP over its stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        /// Extra environment for the child. Entries here win over the inherited
        /// environment and over the recovered login-shell `PATH`.
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP, the transport current MCP servers use.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        /// Keychain account under which this server's OAuth credentials live.
        ///
        /// `None` keeps tokens in memory for the life of the session, which is
        /// only useful for a one-off connection — persistence is the whole
        /// reason the remote path exists.
        credential_key: Option<String>,
    },
}

/// A cheap, cloneable handle to a live session.
///
/// Dropping every clone shuts the session down.
#[derive(Debug, Clone)]
pub struct Handle {
    tx: mpsc::Sender<session::Cmd>,
    events: broadcast::Sender<Event>,
}

impl Handle {
    /// Open a session and complete the MCP handshake.
    ///
    /// The returned [`ServerPeerInfo`] is the server's own description of
    /// itself, so a caller can render the header before any list call returns.
    pub async fn connect(transport: &Transport) -> Result<(Self, Arc<ServerPeerInfo>)> {
        Self::connect_verbose(transport).await.0
    }

    /// Dial, and hand back everything the attempt produced either way.
    ///
    /// Two reasons this exists rather than a `backlog()` on the handle. A
    /// caller cannot subscribe until `connect` returns, and a broadcast send
    /// with no receiver is dropped rather than buffered — so the `initialize`
    /// exchange would always be unrecoverable. And a *failed* connect returns
    /// no handle at all, which is the case where the child's `stderr` holds
    /// the only explanation there is: `npx` not found, a missing API key the
    /// server complains about and exits over. Discarding the transcript
    /// exactly when the connection failed is discarding the diagnosis.
    pub async fn connect_verbose(
        transport: &Transport,
    ) -> (Result<(Self, Arc<ServerPeerInfo>)>, Vec<Event>) {
        let (events, mut early) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (tx, rx) = mpsc::channel(32);
        let spawned = session::spawn(transport, rx, events.clone()).await;

        let mut backlog = Vec::new();
        while let Ok(event) = early.try_recv() {
            backlog.push(event);
        }

        let result = spawned.map(|info| (Self { tx, events }, info));
        (result, backlog)
    }

    /// Subscribe to notifications from the server.
    ///
    /// Only messages sent after subscribing are delivered;
    /// [`Handle::connect_verbose`] has what happened before that.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Whether two handles drive the same session.
    ///
    /// Lets a watcher tell "my session died" from "a newer session already
    /// replaced it" — the two look identical from the server id alone.
    pub fn same_session(&self, other: &Handle) -> bool {
        self.tx.same_channel(&other.tx)
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        self.request(session::Cmd::ListTools).await
    }

    pub async fn list_resources(&self) -> Result<Vec<Resource>> {
        self.request(session::Cmd::ListResources).await
    }

    pub async fn list_prompts(&self) -> Result<Vec<Prompt>> {
        self.request(session::Cmd::ListPrompts).await
    }

    /// Fetch everything that makes up the server's contract, in one round of
    /// concurrent list calls, as a [`Snapshot`] ready to diff or store.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.request(session::Cmd::Snapshot).await
    }

    pub async fn call_tool(&self, name: &str, arguments: JsonObject) -> Result<CallToolResult> {
        let name = name.to_string();
        self.request(move |reply| session::Cmd::CallTool {
            name,
            arguments,
            reply,
        })
        .await
    }

    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        let uri = uri.to_string();
        self.request(move |reply| session::Cmd::ReadResource { uri, reply })
            .await
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<GetPromptResult> {
        let name = name.to_string();
        self.request(move |reply| session::Cmd::GetPrompt {
            name,
            arguments,
            reply,
        })
        .await
    }

    /// Close the session, cancelling the underlying transport.
    ///
    /// Dropping the last handle does the same thing; this just lets a caller
    /// wait for it.
    pub async fn shutdown(&self) -> Result<()> {
        self.request(session::Cmd::Shutdown).await
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> session::Cmd,
    ) -> Result<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(make(reply_tx))
            .await
            .map_err(|_| Error::Closed)?;
        reply_rx.await.map_err(|_| Error::Closed)?
    }
}
