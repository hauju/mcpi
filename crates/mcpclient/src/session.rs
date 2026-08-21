//! The task that owns the `rmcp` service, and the command protocol reaching it.

use std::collections::HashMap;
use std::sync::Arc;

#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, GetPromptResult, JsonObject,
    ProgressNotificationParam, Prompt, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceUpdatedNotificationParam, ServerPeerInfo, Tool,
};
use rmcp::service::{
    ClientInitializeError, NotificationContext, QuitReason, RoleClient, RunningService,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClientTransportConfig,
};
use rmcp::transport::{
    AuthClient, AuthorizationManager, StreamableHttpClientTransport, TokioChildProcess,
};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::wire::Recording;
use crate::{Error, Result, Transport, oauth, pathenv, snapshot};

/// How long a failed handshake waits for the child's `stderr` to reach the
/// event channel. A child that died has already closed the pipe, so this is
/// only reached by one that is still running — and a diagnosis nobody waits
/// for is worth less than a connect that returns.
const STDERR_FLUSH: std::time::Duration = std::time::Duration::from_millis(500);

type Reply<T> = oneshot::Sender<Result<T>>;

pub(crate) enum Cmd {
    ListTools(Reply<Vec<Tool>>),
    ListResources(Reply<Vec<Resource>>),
    ListPrompts(Reply<Vec<Prompt>>),
    Snapshot(Reply<schemadiff::Snapshot>),
    CallTool {
        name: String,
        arguments: JsonObject,
        reply: Reply<CallToolResult>,
    },
    ReadResource {
        uri: String,
        reply: Reply<ReadResourceResult>,
    },
    GetPrompt {
        name: String,
        arguments: Option<JsonObject>,
        reply: Reply<GetPromptResult>,
    },
    Shutdown(Reply<()>),
}

/// Something the server told us without being asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Log {
        level: String,
        logger: Option<String>,
        data: Value,
    },
    Progress {
        token: String,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
    ResourceUpdated {
        uri: String,
    },
    /// The server's tool list changed — the caller should re-snapshot, which is
    /// how a schema diff surfaces without reconnecting.
    ToolListChanged,
    ResourceListChanged,
    PromptListChanged,
    /// A line the stdio child wrote to its own `stderr`.
    ///
    /// Most stdio servers put their diagnostics here and nowhere else, and a
    /// GUI app has no terminal for them to land in — so without this the one
    /// message explaining a failed spawn is written to a closed pipe.
    Stderr {
        line: String,
    },
    /// One JSON-RPC frame, exactly as it crossed the transport.
    Wire {
        /// `true` for client → server.
        outbound: bool,
        message: Value,
    },
    /// The session ended without being asked to: the transport died underneath
    /// it — a crashed stdio child, a dropped HTTP stream. Terminal: nothing on
    /// this session will ever answer again.
    Disconnected {
        reason: String,
    },
}

/// Forwards server notifications onto a broadcast channel.
///
/// Wired up from the first connection rather than when a listener appears:
/// retrofitting a handler onto a live session is not possible, and a dropped
/// notification cannot be recovered.
#[derive(Clone)]
struct Forwarder {
    events: broadcast::Sender<Event>,
}

impl Forwarder {
    /// A send with no subscribers is not an error, just nobody listening yet.
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

impl ClientHandler for Forwarder {
    // SEP-2577 deprecates MCP logging, but servers in the field still emit it
    // and dropping the messages would leave the log pane empty against exactly
    // the servers people are debugging today. Forward it until rmcp removes the
    // type, then delete this method.
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.emit(Event::Log {
            level: format!("{:?}", params.level).to_lowercase(),
            logger: params.logger,
            data: params.data,
        });
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.emit(Event::Progress {
            token: format!("{}", params.progress_token.0),
            progress: params.progress,
            total: params.total,
            message: params.message,
        });
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.emit(Event::ResourceUpdated { uri: params.uri });
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(Event::ToolListChanged);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(Event::ResourceListChanged);
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(Event::PromptListChanged);
    }
}

pub(crate) async fn spawn(
    transport: &Transport,
    rx: mpsc::Receiver<Cmd>,
    events: broadcast::Sender<Event>,
) -> Result<Arc<ServerPeerInfo>> {
    let handler = Forwarder {
        events: events.clone(),
    };

    let service = match transport {
        Transport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let (child, stderr) = build_child(command, args, env, cwd.as_deref()).await?;
            let pump = stderr.map(|stderr| drain_stderr(stderr, events.clone()));
            match handler.serve(Recording::new(child, events.clone())).await {
                Ok(service) => service,
                Err(e) => {
                    // The child's complaint is the only account of why this
                    // failed, and it is still somewhere between the pipe and
                    // the event channel: the pump is a detached task, so
                    // returning here would race it and `connect_verbose` would
                    // drain an empty channel. A failed spawn has usually
                    // already exited, which closes the pipe and ends the pump
                    // at once; the timeout is only for a child that failed to
                    // handshake but is still running.
                    if let Some(pump) = pump {
                        let _ = tokio::time::timeout(STDERR_FLUSH, pump).await;
                    }
                    return Err(Error::Connect(e.to_string()));
                }
            }
        }
        Transport::Http {
            url,
            headers,
            credential_key,
        } => {
            let transport = build_http(url, headers, credential_key.as_deref()).await?;
            handler
                .serve(Recording::new(transport, events.clone()))
                .await
                .map_err(|e| classify(e, url))?
        }
    };

    let info = service
        .peer_info()
        .ok_or_else(|| Error::Connect("the server completed no handshake".into()))?;

    tokio::spawn(run(service, rx, events));
    Ok(info)
}

/// Turn an initialize failure into something the UI can offer a button for.
///
/// The interesting case is a 401: `AuthClient` sends the first request
/// unauthenticated when it has no credentials, so a server that needs sign-in
/// announces itself here rather than up front.
fn classify(error: ClientInitializeError, url: &str) -> Error {
    if let Some(challenge) = auth_challenge(&error) {
        return Error::AuthRequired {
            challenge: Some(challenge),
        };
    }

    // A server still on the deprecated two-endpoint transport will never
    // complete a Streamable HTTP handshake. Saying so beats a bare connection
    // error the user cannot act on.
    if looks_like_legacy_sse(url) {
        Error::LegacySseTransport
    } else {
        Error::Connect(root_cause(&error))
    }
}

/// The innermost cause, which is the only part a person can act on.
///
/// The outer layers spell out rmcp's generic transport types —
/// `WorkerTransport<StreamableHttpClientWorker<AuthClient<Client>>>` and so on —
/// which fill the pane without saying anything. The bottom of the chain is
/// where "connection refused" or "certificate expired" lives.
fn root_cause(error: &ClientInitializeError) -> String {
    let mut deepest: &(dyn std::error::Error + 'static) = match error {
        ClientInitializeError::TransportError { error, .. } => error.error.as_ref(),
        other => other,
    };
    while let Some(next) = deepest.source() {
        deepest = next;
    }
    deepest.to_string()
}

/// Dig the `WWW-Authenticate` challenge out of a failed handshake.
///
/// `ClientInitializeError::TransportError` holds its `DynamicTransportError` in
/// a field named `error` with no `#[source]` attribute, so `source()` returns
/// `None` there and a plain walk of the chain stops one level above everything
/// interesting. The variant is matched explicitly to step across that gap; from
/// the boxed transport error inwards the chain is intact again.
fn auth_challenge(error: &ClientInitializeError) -> Option<String> {
    let start: &(dyn std::error::Error + 'static) = match error {
        ClientInitializeError::TransportError { error, .. } => error.error.as_ref(),
        other => other,
    };

    let mut current = Some(start);
    while let Some(link) = current {
        if let Some(auth) = link.downcast_ref::<AuthRequiredError>() {
            return Some(auth.www_authenticate_header.clone());
        }
        current = link.source();
    }
    None
}

/// The deprecated transport is conventionally mounted at `/sse`.
fn looks_like_legacy_sse(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .ends_with("/sse")
}

/// Spawn the child, returning its transport and its `stderr`.
///
/// rmcp's default is `Stdio::inherit()`, which in a bundled GUI app means the
/// server's diagnostics go to a console nobody is looking at. Piping it is the
/// difference between "failed to start" and the line saying why.
async fn build_child(
    command: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: Option<&std::path::Path>,
) -> Result<(TokioChildProcess, Option<tokio::process::ChildStderr>)> {
    // The caller's own PATH wins, then the login shell's, then whatever this
    // process inherited.
    let path = match env.get("PATH") {
        Some(explicit) => Some(explicit.clone()),
        None => pathenv::login_shell_path().await,
    };

    let program = pathenv::resolve_command(command, path.as_deref()).ok_or_else(|| {
        Error::CommandNotFound {
            command: command.to_string(),
            searched: path
                .clone()
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_else(|| "<no PATH>".into()),
        }
    })?;

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(args);
    if let Some(path) = &path {
        cmd.env("PATH", path);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            command: command.to_string(),
            source,
        })
}

/// Forward the child's `stderr` onto the event channel, one line at a time.
///
/// The task ends when the pipe closes, which is when the child exits — so it
/// needs no shutdown signal of its own. The handle comes back so a failed
/// handshake can wait for the pump to finish before anyone reads the channel.
fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    events: broadcast::Sender<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        // A server that dies mid-line still said something; `next_line` yields
        // the trailing fragment before it reports the end of the stream.
        while let Ok(Some(line)) = lines.next_line().await {
            // A send with no subscribers is not an error, just nobody
            // listening yet — and the first lines, written before the UI
            // attaches, are the ones that explain a bad spawn.
            let _ = events.send(Event::Stderr { line });
        }
    })
}

/// Every remote connection goes through `AuthClient`, whether or not the server
/// wants OAuth.
///
/// It sends the first request unauthenticated when there are no credentials, so
/// a server that needs none is unaffected, and one that does answers with a 401
/// carrying the challenge that drives sign-in. One code path, and token refresh
/// on a rejected token comes for free.
async fn build_http(
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    credential_key: Option<&str>,
) -> Result<StreamableHttpClientTransport<AuthClient<reqwest::Client>>> {
    let mut custom = HashMap::new();
    for (key, value) in headers {
        let name: reqwest::header::HeaderName = key
            .parse()
            .map_err(|_| Error::Connect(format!("`{key}` is not a valid header name")))?;
        let value: reqwest::header::HeaderValue = value
            .parse()
            .map_err(|_| Error::Connect(format!("the value for `{key}` is not a valid header")))?;
        custom.insert(name, value);
    }

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    config.custom_headers = custom;

    let manager = match credential_key {
        Some(key) => oauth::manager_for(url, key).await?,
        // Without a key there is nowhere to persist, so the manager starts and
        // ends empty — sign-in would not survive the session.
        None => AuthorizationManager::new(url)
            .await
            .map_err(|e| Error::Connect(format!("could not prepare authorization: {e}")))?,
    };

    Ok(StreamableHttpClientTransport::with_client(
        AuthClient::new(reqwest::Client::default(), manager),
        config,
    ))
}

/// Dispatch loop.
///
/// Each command is handed to its own task on a cloned peer, so a tool call that
/// takes thirty seconds does not hold up a list refresh queued behind it.
///
/// The loop also watches the service itself. When the transport dies underneath
/// it — a crashed stdio child, a dropped HTTP stream — nothing dispatched on the
/// peer will ever answer again, and `Event::Disconnected` says so; without it a
/// dead session keeps reading as connected until the next call happens to fail.
async fn run(
    service: RunningService<RoleClient, Forwarder>,
    mut rx: mpsc::Receiver<Cmd>,
    events: broadcast::Sender<Event>,
) {
    let session_peer = service.peer().clone();
    let cancel = service.cancellation_token();
    // Consumes the service: from here `done` is its liveness, `session_peer`
    // its dispatch surface (which also carries the peer info).
    let mut done = std::pin::pin!(service.waiting());

    loop {
        let cmd = tokio::select! {
            quit = &mut done => {
                let reason = match quit {
                    Ok(QuitReason::Closed) => "the server closed the connection".to_string(),
                    Ok(other) => format!("{other:?}"),
                    Err(e) => format!("the session task failed: {e}"),
                };
                let _ = events.send(Event::Disconnected { reason });
                return;
            }
            cmd = rx.recv() => cmd,
        };
        let Some(cmd) = cmd else { break };
        let peer = session_peer.clone();

        match cmd {
            Cmd::Shutdown(reply) => {
                let _ = reply.send(Ok(()));
                break;
            }
            Cmd::ListTools(reply) => {
                tokio::spawn(async move {
                    let _ = reply.send(peer.list_all_tools().await.map_err(request_error));
                });
            }
            Cmd::ListResources(reply) => {
                tokio::spawn(async move {
                    let _ = reply.send(peer.list_all_resources().await.map_err(request_error));
                });
            }
            Cmd::ListPrompts(reply) => {
                tokio::spawn(async move {
                    let _ = reply.send(peer.list_all_prompts().await.map_err(request_error));
                });
            }
            Cmd::Snapshot(reply) => {
                let info = peer.peer_info();
                tokio::spawn(async move {
                    let _ = reply.send(snapshot::collect(&peer, info).await);
                });
            }
            Cmd::CallTool {
                name,
                arguments,
                reply,
            } => {
                tokio::spawn(async move {
                    let params = CallToolRequestParams::new(name).with_arguments(arguments);
                    let _ = reply.send(peer.call_tool(params).await.map_err(request_error));
                });
            }
            Cmd::ReadResource { uri, reply } => {
                tokio::spawn(async move {
                    let params = ReadResourceRequestParams::new(uri);
                    let _ = reply.send(peer.read_resource(params).await.map_err(request_error));
                });
            }
            Cmd::GetPrompt {
                name,
                arguments,
                reply,
            } => {
                tokio::spawn(async move {
                    let mut params = GetPromptRequestParams::new(name);
                    if let Some(arguments) = arguments {
                        params = params.with_arguments(arguments);
                    }
                    let _ = reply.send(peer.get_prompt(params).await.map_err(request_error));
                });
            }
        }
    }

    // Either shutdown was requested or every handle was dropped. Cancelling
    // terminates the transport, which for stdio means reaping the child rather
    // than leaving an orphan behind; awaiting `done` reaps the service task.
    cancel.cancel();
    let _ = done.await;
}

fn request_error(error: rmcp::service::ServiceError) -> Error {
    Error::Request(error.to_string())
}

#[cfg(test)]
mod tests {
    /// Guards the TLS backend.
    ///
    /// `use_rustls_tls` is feature-gated in reqwest, so this stops compiling
    /// the moment the `rustls` feature is dropped from our dependency —
    /// which is how every `https://` server silently became unreachable
    /// while the local `http://127.0.0.1` tests stayed green.
    #[test]
    fn the_http_client_has_a_tls_backend() {
        reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .expect("a TLS-capable client must be constructible");
    }
}
