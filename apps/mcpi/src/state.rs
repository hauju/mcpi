//! Shared application state and the actions that mutate it.
//!
//! Everything reactive lives in signals on [`AppState`], which is provided once
//! at the root and read by every pane. `AppState` is `Copy`, so actions can be
//! called straight from an event handler and moved into a spawned task without
//! any clone ceremony.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use mcpclient::{Event, Handle, Snapshot};
use mcpstore::{
    CallKind, CallRow, Collection, CollectionId, NewCall, NewServer, ServerId, ServerRow,
    SnapshotId, SnapshotOutcome, Step, StepId, Store, StoredSnapshot, TransportKind,
};
use schemadiff::{ItemChange, SnapshotDiff};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::{config, form, import};

/// The hosted client-ID metadata document (CIMD, SEP-991) the OAuth flow
/// offers as its client id. Set at build time, because it names a deployed
/// site rather than anything in this repo. A build without it simply
/// registers dynamically, exactly as before.
const CLIENT_METADATA_URL: Option<&str> = option_env!("CLIENT_METADATA_URL");

/// Which kind of thing the middle pane is listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tools,
    Resources,
    Prompts,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Tools, Tab::Resources, Tab::Prompts];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Tools => "Tools",
            Tab::Resources => "Resources",
            Tab::Prompts => "Prompts",
        }
    }
}

/// The item selected in the middle pane, rendered by the detail pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub tab: Tab,
    /// Tool name, resource URI, or prompt name.
    pub name: String,
}

/// What this connect learned about the server's contract relative to history.
///
/// "Never seen before" and "seen, and identical" are different answers and the
/// header says so — collapsing both into an absent diff would leave a first
/// connect looking like a clean bill of health.
#[derive(Debug, Clone, PartialEq)]
pub enum ContractStatus {
    First,
    Unchanged,
    Changed(Box<SnapshotDiff>),
}

impl ContractStatus {
    pub fn diff(&self) -> Option<&SnapshotDiff> {
        match self {
            Self::Changed(diff) => Some(diff),
            _ => None,
        }
    }
}

/// A diff being shown outside a live connection's status: a past change
/// opened from the snapshot timeline, or the built-in sample.
///
/// This is what keeps the diff an artifact rather than an event: the live diff
/// dies with its connection, but every change is recorded, and this view is
/// how a recorded one gets back on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryView {
    /// Shown under the drawer title: which server, and which two moments.
    pub subtitle: String,
    pub diff: Box<SnapshotDiff>,
}

/// A live session plus everything learned during the handshake.
#[derive(Clone)]
pub struct Connected {
    pub handle: Handle,
    pub snapshot: Snapshot,
    pub status: ContractStatus,
    /// Free-text usage notes the server supplied at initialize.
    pub instructions: Option<String>,
}

impl Connected {
    /// The recorded change for one item, if this connect saw it move.
    pub fn change_for(&self, selection: &Selection) -> Option<ItemChange> {
        let diff = self.status.diff()?;
        let list = match selection.tab {
            Tab::Tools => &diff.tools,
            Tab::Resources => &diff.resources,
            Tab::Prompts => &diff.prompts,
        };
        list.iter().find(|c| c.name() == selection.name).cloned()
    }
}

#[derive(Clone)]
pub enum Conn {
    Disconnected,
    Connecting,
    Connected(Box<Connected>),
    /// The server answered the first unauthenticated request with a 401.
    /// `challenge` is its `WWW-Authenticate` header, which tells the OAuth
    /// flow where to find the authorization server.
    NeedsAuth {
        challenge: Option<String>,
    },
    /// Signing in: the browser is open and we are waiting on the redirect.
    Authorizing,
    Failed(String),
}

/// The part of [`Conn`] a component can be handed as a prop.
///
/// `Conn` itself holds a live `Handle`, which has no meaningful equality, so it
/// cannot be a prop — and a status dot has no business owning a session anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
    NeedsAuth,
    Failed,
}

impl Conn {
    pub fn connected(&self) -> Option<&Connected> {
        match self {
            Conn::Connected(c) => Some(c),
            _ => None,
        }
    }

    pub fn status(&self) -> Status {
        match self {
            Conn::Disconnected => Status::Disconnected,
            Conn::Connecting | Conn::Authorizing => Status::Connecting,
            Conn::Connected(_) => Status::Connected,
            Conn::NeedsAuth { .. } => Status::NeedsAuth,
            Conn::Failed(_) => Status::Failed,
        }
    }
}

/// A server being added or edited, held as text so the form stays trivial.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerDraft {
    /// `None` while adding, `Some` while editing an existing row.
    pub id: Option<ServerId>,
    pub name: String,
    pub kind: DraftKind,
    pub command: String,
    /// One argument per line.
    pub args: String,
    /// `KEY=value` per line.
    pub env: String,
    pub cwd: String,
    pub url: String,
    /// `Header: value` per line.
    pub headers: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DraftKind {
    /// The default for a new server: remote endpoints are what people paste in,
    /// and a local command is the deliberate choice of the two.
    #[default]
    Http,
    Stdio,
}

impl From<DraftKind> for TransportKind {
    fn from(kind: DraftKind) -> Self {
        match kind {
            DraftKind::Stdio => TransportKind::Stdio,
            DraftKind::Http => TransportKind::Http,
        }
    }
}

impl ServerDraft {
    pub fn from_row(row: &ServerRow) -> Self {
        let mut draft = Self {
            id: Some(row.id),
            name: row.name.clone(),
            ..Default::default()
        };
        match row.transport_kind {
            TransportKind::Stdio => {
                draft.kind = DraftKind::Stdio;
                if let Ok(c) = serde_json::from_value::<config::StdioConfig>(row.config.clone()) {
                    draft.command = c.command;
                    draft.args = c.args.join("\n");
                    draft.env = config::unpairs(&c.env, "=");
                    draft.cwd = c.cwd.unwrap_or_default();
                }
            }
            TransportKind::Http => {
                draft.kind = DraftKind::Http;
                if let Ok(c) = serde_json::from_value::<config::HttpConfig>(row.config.clone()) {
                    draft.url = c.url;
                    draft.headers = config::unpairs(&c.headers, ": ");
                }
            }
        }
        draft
    }

    /// Validate and render the draft into the JSON the store persists.
    pub fn to_config(&self) -> Result<serde_json::Value, String> {
        if self.name.trim().is_empty() {
            return Err("Give the server a name.".into());
        }
        match self.kind {
            DraftKind::Stdio => {
                if self.command.trim().is_empty() {
                    return Err("A stdio server needs a command to run.".into());
                }
                serde_json::to_value(config::StdioConfig {
                    command: self.command.trim().to_string(),
                    args: config::lines(&self.args),
                    env: config::pairs(&self.env, '='),
                    cwd: Some(self.cwd.trim().to_string()).filter(|c| !c.is_empty()),
                })
            }
            DraftKind::Http => {
                let url = self.url.trim();
                if url.is_empty() {
                    return Err("A remote server needs a URL.".into());
                }
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err("The URL must start with http:// or https://".into());
                }
                serde_json::to_value(config::HttpConfig {
                    url: url.to_string(),
                    headers: config::pairs(&self.headers, ':'),
                })
            }
        }
        .map_err(|e| e.to_string())
    }
}

/// One entry in a session's console, in the order it happened.
///
/// Frames, `stderr`, and notifications share a single stream rather than three
/// tabs: the useful question is almost always "what happened around the time
/// this broke", and answering it out of three separately-scrolled panes means
/// reconstructing the interleaving by eye.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleLine {
    pub kind: LineKind,
    /// The collapsed one-liner.
    pub summary: String,
    /// The body, when there is one worth expanding into.
    pub detail: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A JSON-RPC frame this client sent.
    Sent,
    /// A JSON-RPC frame the server sent.
    Received,
    /// A line the stdio child wrote to its own `stderr`.
    Stderr,
    /// An MCP `notifications/message` from the server.
    Log,
    /// Something the session itself reports: progress, a list change, death.
    Notice,
}

impl LineKind {
    pub fn label(self) -> &'static str {
        match self {
            LineKind::Sent => "sent",
            LineKind::Received => "recv",
            LineKind::Stderr => "stderr",
            LineKind::Log => "log",
            LineKind::Notice => "session",
        }
    }
}

impl ConsoleLine {
    /// Render one session event as a line.
    ///
    /// Every frame keeps its full body in `detail`; the summary only decides
    /// what you can read without expanding it.
    fn from_event(event: &Event) -> Self {
        match event {
            Event::Wire { outbound, message } => Self {
                kind: if *outbound {
                    LineKind::Sent
                } else {
                    LineKind::Received
                },
                summary: frame_summary(message),
                detail: Some(message.clone()),
            },
            Event::Stderr { line } => Self {
                kind: LineKind::Stderr,
                summary: line.clone(),
                detail: None,
            },
            Event::Log {
                level,
                logger,
                data,
            } => Self {
                kind: LineKind::Log,
                summary: match logger {
                    Some(logger) => format!("[{level}] {logger}"),
                    None => format!("[{level}]"),
                },
                detail: Some(data.clone()),
            },
            Event::Progress {
                token,
                progress,
                total,
                message,
            } => Self {
                kind: LineKind::Notice,
                summary: match (total, message) {
                    (Some(total), Some(message)) => {
                        format!("progress {progress}/{total} - {message}")
                    }
                    (Some(total), None) => format!("progress {progress}/{total}"),
                    (None, Some(message)) => format!("progress {progress} - {message}"),
                    (None, None) => format!("progress {progress} (token {token})"),
                },
                detail: None,
            },
            Event::ResourceUpdated { uri } => Self {
                kind: LineKind::Notice,
                summary: format!("resource updated: {uri}"),
                detail: None,
            },
            Event::ToolListChanged => Self::notice("tool list changed"),
            Event::ResourceListChanged => Self::notice("resource list changed"),
            Event::PromptListChanged => Self::notice("prompt list changed"),
            Event::Disconnected { reason } => Self::notice(&format!("disconnected: {reason}")),
        }
    }

    pub(crate) fn notice(summary: &str) -> Self {
        Self {
            kind: LineKind::Notice,
            summary: summary.to_string(),
            detail: None,
        }
    }
}

/// What a JSON-RPC frame is, in one line.
///
/// A request is named by its method; a response carries none, so it is
/// identified by the id it answers - which is what makes a request/response
/// pair legible in a flat stream without pairing them up structurally.
fn frame_summary(message: &Value) -> String {
    let id = message
        .get("id")
        .filter(|id| !id.is_null())
        .map(|id| format!(" #{id}"))
        .unwrap_or_default();

    if let Some(method) = message.get("method").and_then(Value::as_str) {
        // The target is the reason you are reading the line at all: five
        // `tools/call` frames in a row are otherwise indistinguishable.
        let target = message
            .get("params")
            .and_then(|p| p.get("name").or_else(|| p.get("uri")))
            .and_then(Value::as_str)
            .map(|t| format!(" {t}"))
            .unwrap_or_default();
        return format!("{method}{target}{id}");
    }
    if let Some(error) = message.get("error") {
        let code = error.get("code").map(|c| c.to_string()).unwrap_or_default();
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error");
        return format!("error{id}: {code} {text}");
    }
    format!("result{id}")
}

/// A banner message with a severity: failures are loud, confirmations are
/// not. Without the split, "Added echo to the collection." renders in the same
/// red bar as a connection error and reads as one.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub error: bool,
    pub text: String,
}

impl Notice {
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            error: true,
            text: text.into(),
        }
    }

    pub fn info(text: impl Into<String>) -> Self {
        Self {
            error: false,
            text: text.into(),
        }
    }
}

/// `Copy`, so it drops into every event handler and async task without a
/// clone. That is why the store sits behind a `Signal` rather than being a bare
/// `Arc` field — signals are `Copy`, `Arc` is not, and one non-`Copy` field
/// would force a manual clone at every call site.
#[derive(Clone, Copy)]
pub struct AppState {
    store: Signal<Arc<Store>>,
    pub servers: Signal<Vec<ServerRow>>,
    pub conns: Signal<HashMap<ServerId, Conn>>,
    pub selected_server: Signal<Option<ServerId>>,
    pub selection: Signal<Option<Selection>>,
    pub tab: Signal<Tab>,
    /// `Some` while the add/edit dialog is open.
    pub draft: Signal<Option<ServerDraft>>,
    /// Transient banner for messages that belong to no particular pane.
    pub notice: Signal<Option<Notice>>,
    /// Arguments being composed for the selected item.
    pub form: Signal<Value>,
    /// Whether the form is showing its raw JSON editor.
    pub raw_form: Signal<bool>,
    /// The last call's result, shown under the form.
    pub outcome: Signal<Option<CallOutcome>>,
    pub running: Signal<bool>,
    /// Recent calls against the selected server, newest first.
    pub history: Signal<Vec<CallRow>>,
    /// Tools on the selected server whose last call was refused for want of
    /// credentials, and when. Refreshed alongside `history` because it is read
    /// out of the same call log; the other half of the lock — a description
    /// that merely claims auth is needed — is derived at render time from the
    /// snapshot, and never gets stored next to something we actually observed.
    pub auth_failures: Signal<HashMap<String, DateTime<Utc>>>,
    /// Diagnosis of the URL in the server dialog, when one has been run.
    pub endpoint_report: Signal<Option<probe::Report>>,
    pub probing: Signal<bool>,
    /// Whether the ⌘K palette is open.
    pub palette_open: Signal<bool>,
    /// Whether the full contract-diff modal is open.
    pub diff_open: Signal<bool>,
    /// Whether the "import from another client" dialog is open.
    pub import_open: Signal<bool>,
    /// Per-server session console: frames, `stderr`, and notifications, oldest
    /// first. Kept per server rather than globally so switching servers does
    /// not mean scrolling past another connection's traffic.
    pub console: Signal<HashMap<ServerId, Vec<ConsoleLine>>>,
    /// Whether the console drawer is open.
    pub console_open: Signal<bool>,
    /// Recorded contract snapshots for the selected server, newest first.
    /// Rows are only written when the contract moved, so consecutive pairs
    /// are real change events, not connection noise.
    pub snapshots: Signal<Vec<StoredSnapshot>>,
    /// The selected server's named baselines. Kept apart from `snapshots`:
    /// a pin can be older than the timeline window, and splicing it into the
    /// list would fabricate a "consecutive" pair that skips real changes.
    pub baselines: Signal<Vec<StoredSnapshot>>,
    /// A past diff being viewed in the drawer, taking precedence over the
    /// live connection's status. Cleared when the drawer closes.
    pub history_view: Signal<Option<HistoryView>>,
    /// Saved call sequences for the selected server.
    pub collections: Signal<Vec<Collection>>,
    /// The collection whose runner is open, if any.
    pub open_collection: Signal<Option<CollectionId>>,
    pub steps: Signal<Vec<Step>>,
    /// Results of the last run, in step order.
    pub run_results: Signal<Vec<StepOutcome>>,
    pub running_collection: Signal<bool>,
}

/// One step's result within a collection run.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    pub step_id: StepId,
    pub target: String,
    pub is_error: bool,
    pub duration_ms: i64,
    pub response: Value,
}

/// A finished call: what was sent, what came back, and how long it took.
#[derive(Debug, Clone, PartialEq)]
pub struct CallOutcome {
    pub target: String,
    pub request: Value,
    pub response: Value,
    pub is_error: bool,
    pub duration_ms: i64,
}

/// How many calls the sidebar's history section keeps in view.
const HISTORY_LIMIT: usize = 50;

/// How many console lines one session keeps. A chatty server can emit
/// thousands; the recent ones are what a person is reading, and an unbounded
/// transcript in a `Signal` is an unbounded diff on every render.
const CONSOLE_LIMIT: usize = 1000;

/// How many recorded snapshots the timeline keeps in view. Rows only exist
/// where the contract moved, so even a busy server accumulates these slowly.
const SNAPSHOT_LIMIT: usize = 20;

impl AppState {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store: Signal::new(store),
            servers: Signal::new(Vec::new()),
            conns: Signal::new(HashMap::new()),
            selected_server: Signal::new(None),
            selection: Signal::new(None),
            tab: Signal::new(Tab::Tools),
            draft: Signal::new(None),
            notice: Signal::new(None),
            form: Signal::new(Value::Object(Default::default())),
            raw_form: Signal::new(false),
            outcome: Signal::new(None),
            running: Signal::new(false),
            history: Signal::new(Vec::new()),
            auth_failures: Signal::new(HashMap::new()),
            endpoint_report: Signal::new(None),
            probing: Signal::new(false),
            palette_open: Signal::new(false),
            diff_open: Signal::new(false),
            import_open: Signal::new(false),
            console: Signal::new(HashMap::new()),
            console_open: Signal::new(false),
            snapshots: Signal::new(Vec::new()),
            baselines: Signal::new(Vec::new()),
            history_view: Signal::new(None),
            collections: Signal::new(Vec::new()),
            open_collection: Signal::new(None),
            steps: Signal::new(Vec::new()),
            run_results: Signal::new(Vec::new()),
            running_collection: Signal::new(false),
        }
    }

    /// Clone the `Arc` straight out rather than returning a guard: holding a
    /// signal borrow across an `await` is how "already borrowed" panics happen.
    fn store(&self) -> Arc<Store> {
        self.store.read().clone()
    }

    pub fn conn(&self, id: ServerId) -> Conn {
        self.conns
            .read()
            .get(&id)
            .cloned()
            .unwrap_or(Conn::Disconnected)
    }

    /// The connection for whichever server is selected, if it is live.
    pub fn active(&self) -> Option<Connected> {
        let id = (*self.selected_server.read())?;
        self.conns.read().get(&id)?.connected().cloned()
    }

    pub fn reload_servers(&self) {
        let mut servers = self.servers;
        let mut notice = self.notice;
        match self.store().list_servers() {
            Ok(rows) => servers.set(rows),
            Err(e) => notice.set(Some(Notice::error(format!(
                "Could not read the server list: {e}"
            )))),
        }
    }

    fn set_conn(&self, id: ServerId, state: Conn) {
        let mut conns = self.conns;
        conns.write().insert(id, state);
    }

    pub fn select_server(&self, id: ServerId) {
        let mut selected = self.selected_server;
        selected.set(Some(id));
        // The previous selection named an item on a different server.
        self.clear_selection();
        self.reload_history();
        self.reload_collections();
        self.reload_snapshots();
    }

    /// Back to the server summary. Public because the detail pane offers it:
    /// without a way out of a selection, the snapshot timeline and baseline
    /// pinning become unreachable for the rest of the session after one click.
    pub fn clear_selection(&self) {
        let mut selection = self.selection;
        let mut form = self.form;
        let mut outcome = self.outcome;
        let mut raw = self.raw_form;
        selection.set(None);
        form.set(Value::Object(Default::default()));
        outcome.set(None);
        raw.set(false);
    }

    /// Select an item and seed the form from its schema.
    ///
    /// Seeding here rather than in an effect keeps the ordering obvious: one
    /// user action, one synchronous state change.
    pub fn select_item(&self, next: Selection) {
        let mut selection = self.selection;
        let mut form = self.form;
        let mut outcome = self.outcome;

        form.set(
            self.schema_for(&next)
                .map(|schema| form::seed(&schema))
                .unwrap_or_else(|| Value::Object(Default::default())),
        );
        outcome.set(None);
        selection.set(Some(next));
    }

    /// Move the selection one row within the active tab, wrapping at the ends.
    /// Driven by the global arrow-key listener; the order matches the browser
    /// list because both walk the same sorted snapshot keys.
    pub fn select_adjacent(&self, step: i64) {
        let Some(connected) = self.active() else {
            return;
        };
        let tab = *self.tab.read();
        let names: Vec<String> = match tab {
            Tab::Tools => connected.snapshot.tools.keys().cloned().collect(),
            Tab::Resources => connected.snapshot.resources.keys().cloned().collect(),
            Tab::Prompts => connected.snapshot.prompts.keys().cloned().collect(),
        };
        if names.is_empty() {
            return;
        }

        let current = self
            .selection
            .read()
            .clone()
            .filter(|s| s.tab == tab)
            .and_then(|s| names.iter().position(|n| *n == s.name));
        let next = match current {
            Some(i) => (i as i64 + step).rem_euclid(names.len() as i64) as usize,
            None if step > 0 => 0,
            None => names.len() - 1,
        };
        self.select_item(Selection {
            tab,
            name: names[next].clone(),
        });
    }

    /// Whether anything modal is on screen. The global arrow-key listener
    /// checks this so list navigation never fires behind an open dialog.
    pub fn overlay_open(&self) -> bool {
        *self.palette_open.read()
            || self.draft.read().is_some()
            || *self.diff_open.read()
            || *self.import_open.read()
            || self.history_view.read().is_some()
            || self.open_collection.read().is_some()
    }

    /// The schema driving the form for a selection.
    ///
    /// Tools carry one directly. Prompts describe arguments as a flat list, so
    /// one is synthesised. Resources take no arguments at all.
    pub fn schema_for(&self, selection: &Selection) -> Option<Value> {
        let connected = self.active()?;
        match selection.tab {
            Tab::Tools => connected
                .snapshot
                .tools
                .get(&selection.name)?
                .get("inputSchema")
                .cloned(),
            Tab::Prompts => Some(form::schema_for_prompt_arguments(
                connected
                    .snapshot
                    .prompts
                    .get(&selection.name)?
                    .get("arguments"),
            )),
            Tab::Resources => None,
        }
    }

    pub fn reload_history(&self) {
        let mut history = self.history;
        let Some(id) = *self.selected_server.read() else {
            history.set(Vec::new());
            return;
        };
        match self.store().calls(Some(id), HISTORY_LIMIT) {
            Ok(rows) => history.set(rows),
            Err(e) => {
                let mut notice = self.notice;
                notice.set(Some(Notice::error(format!(
                    "Could not read call history: {e}"
                ))));
            }
        }
        self.reload_auth_failures(id);
    }

    /// Which tools last refused for want of credentials. Read from the whole
    /// call log rather than the loaded `history` window: a lock earned fifty
    /// calls ago is exactly the one worth still showing.
    ///
    /// A failure to read this is swallowed — the lock is a hint, and losing it
    /// is not worth a banner over the pane the user is trying to work in.
    fn reload_auth_failures(&self, id: ServerId) {
        let mut auth_failures = self.auth_failures;
        let Ok(rows) = self.store().last_call_failures(id) else {
            return;
        };
        auth_failures.set(
            rows.into_iter()
                .filter(|(_, response, _)| crate::auth_hint::error_is_auth(response))
                .map(|(target, _, at)| (target, at))
                .collect(),
        );
    }

    pub fn reload_snapshots(&self) {
        let mut snapshots = self.snapshots;
        let mut baselines = self.baselines;
        let Some(id) = *self.selected_server.read() else {
            snapshots.set(Vec::new());
            baselines.set(Vec::new());
            return;
        };
        let store = self.store();
        match store.snapshots(id, SNAPSHOT_LIMIT) {
            Ok(rows) => snapshots.set(rows),
            Err(e) => {
                let mut notice = self.notice;
                notice.set(Some(Notice::error(format!(
                    "Could not read the snapshot history: {e}"
                ))));
            }
        }
        baselines.set(store.labeled_snapshots(id).unwrap_or_default());
    }

    /// Pin a name ("v1.2") on a recorded snapshot, or clear it with `None`.
    pub fn set_baseline(&self, id: SnapshotId, label: Option<&str>) {
        let label = label.map(str::trim).filter(|l| !l.is_empty());
        let mut notice = self.notice;
        // Checked here for a readable message; the store's unique index is
        // the backstop.
        if let Some(name) = label
            && self
                .baselines
                .read()
                .iter()
                .any(|s| s.id != id && s.label.as_deref() == Some(name))
        {
            notice.set(Some(Notice::error(format!(
                "“{name}” already names another snapshot."
            ))));
            return;
        }
        match self.store().set_snapshot_label(id, label) {
            Ok(()) => self.reload_snapshots(),
            Err(e) => notice.set(Some(Notice::error(format!(
                "Could not save the baseline: {e}"
            )))),
        }
    }

    /// Open the drawer on the change between two recorded snapshots.
    ///
    /// The diff is computed here, on demand, rather than persisted — the
    /// snapshots are the record, and `schemadiff::diff` is pure and cheap.
    pub fn open_history_diff(&self, before: &StoredSnapshot, after: &StoredSnapshot) {
        let server_name = self
            .servers
            .read()
            .iter()
            .find(|s| s.id == after.server_id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        // A pinned name is more meaningful than a timestamp, and this line
        // also becomes the heading of an exported diff.
        let stamp = |s: &StoredSnapshot| {
            let when = s
                .taken_at
                .with_timezone(&chrono::Local)
                .format("%-d %b %Y %H:%M");
            match &s.label {
                Some(label) => format!("{when} ({label})"),
                None => when.to_string(),
            }
        };
        let subtitle = format!("{server_name} · {} → {}", stamp(before), stamp(after));
        let diff = schemadiff::diff(&before.snapshot, &after.snapshot);
        let mut view = self.history_view;
        view.set(Some(HistoryView {
            subtitle,
            diff: Box::new(diff),
        }));
        let mut open = self.diff_open;
        open.set(true);
    }

    /// Open the drawer on the built-in sample change.
    ///
    /// This exists because the diff only demonstrates itself when a server
    /// actually changes, which can take months — the sample is how someone
    /// sees the feature without waiting for one.
    pub fn open_sample_diff(&self) {
        let mut view = self.history_view;
        view.set(Some(crate::demo::sample_view()));
        let mut open = self.diff_open;
        open.set(true);
    }

    /// Run the current selection with the current form arguments.
    pub fn run(&self) {
        let app = *self;
        let (Some(id), Some(selection)) =
            (*self.selected_server.read(), self.selection.read().clone())
        else {
            return;
        };
        let Some(connected) = self.active() else {
            return;
        };
        let request = self.form.read().clone();

        spawn(async move {
            let mut running = app.running;
            running.set(true);

            let started = std::time::Instant::now();
            // Channel-only today, since the session's IO lives on its own task
            // — but routed through `off_scheduler` anyway so the rule holds
            // without anyone having to re-derive rmcp's internals.
            let result = {
                let (handle, selection, request) =
                    (connected.handle.clone(), selection.clone(), request.clone());
                match off_scheduler(async move { execute(&handle, &selection, &request).await })
                    .await
                {
                    Ok(result) => result,
                    Err(e) => Err(mcpclient::Error::Connect(e)),
                }
            };
            // Measured around the call itself, not around the render, so the
            // number means what a user would assume it means.
            let duration_ms = started.elapsed().as_millis() as i64;

            let (response, is_error) = match result {
                Ok(response) => {
                    // A tool reporting failure through `isError` is not a
                    // transport failure, and the pane has to tell them apart.
                    let flagged = response
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    (response, flagged)
                }
                Err(e) => (serde_json::json!({ "error": e.to_string() }), true),
            };

            let mut outcome = app.outcome;
            outcome.set(Some(CallOutcome {
                target: selection.name.clone(),
                request: request.clone(),
                response: response.clone(),
                is_error,
                duration_ms,
            }));
            running.set(false);

            let recorded = app.store().record_call(NewCall {
                server_id: id,
                kind: kind_of(selection.tab),
                target: selection.name.clone(),
                request,
                response,
                is_error,
                duration_ms,
            });
            if let Err(e) = recorded {
                let mut notice = app.notice;
                // The result is on screen either way; only history is lost.
                notice.set(Some(Notice::error(format!(
                    "The call ran but was not saved to history: {e}"
                ))));
            }
            app.reload_history();
        });
    }

    /// Load a past call back into the form without running it.
    pub fn load_from_history(&self, call: &CallRow) {
        let mut selection = self.selection;
        let mut form = self.form;
        let mut tab = self.tab;
        let mut outcome = self.outcome;

        let next = Selection {
            tab: tab_of(&call.kind),
            name: call.target.clone(),
        };
        tab.set(next.tab);
        selection.set(Some(next));
        form.set(call.request.clone());
        // Show what it returned last time until it is run again.
        outcome.set(Some(CallOutcome {
            target: call.target.clone(),
            request: call.request.clone(),
            response: call.response.clone(),
            is_error: call.is_error,
            duration_ms: call.duration_ms,
        }));
    }

    pub fn replay(&self, call: &CallRow) {
        self.load_from_history(call);
        self.run();
    }

    pub fn connect(&self, id: ServerId) {
        let app = *self;
        spawn(async move {
            // Every `.await` on IO below goes through `off_scheduler`; see its
            // documentation for why, and never add a bare one.
            let row = match app.store().get_server(id) {
                Ok(row) => row,
                Err(e) => return app.set_conn(id, Conn::Failed(e.to_string())),
            };

            let transport = match config::to_transport(row.transport_kind, &row.config, row.id) {
                Ok(transport) => transport,
                Err(e) => {
                    return app.set_conn(
                        id,
                        Conn::Failed(format!("Saved settings are unreadable: {e}")),
                    );
                }
            };

            app.set_conn(id, Conn::Connecting);

            let dialled =
                match off_scheduler(async move { Handle::connect_verbose(&transport).await }).await
                {
                    Ok(dialled) => dialled,
                    Err(e) => return app.set_conn(id, Conn::Failed(e)),
                };
            let (result, backlog) = dialled;

            // Seeded before the outcome is inspected, and reset rather than
            // appended to: the console belongs to a session, and a failed
            // handshake is exactly the case where the child's own words on
            // `stderr` are the only diagnosis that exists.
            app.reset_console(id, &backlog);

            let (handle, info) = match result {
                Ok(pair) => pair,
                // Not a failure to report — an action to offer.
                Err(mcpclient::Error::AuthRequired { challenge }) => {
                    return app.set_conn(id, Conn::NeedsAuth { challenge });
                }
                Err(e) => return app.set_conn(id, Conn::Failed(e.to_string())),
            };
            // Subscribed before anything else awaits: a notification fired
            // during the first snapshot must land in the channel, not vanish.
            let listener = handle.events();

            let snapshot = {
                let handle = handle.clone();
                match off_scheduler(async move { handle.snapshot().await }).await {
                    Ok(Ok(snapshot)) => snapshot,
                    Ok(Err(e)) => return app.set_conn(id, Conn::Failed(e.to_string())),
                    Err(e) => return app.set_conn(id, Conn::Failed(e)),
                }
            };

            // Recording is what turns "connected" into "connected, and here is
            // what changed since last time".
            let status = match app.store().record_snapshot(id, &snapshot) {
                Ok(SnapshotOutcome::Changed { diff, .. }) => ContractStatus::Changed(diff),
                Ok(SnapshotOutcome::First { .. }) => ContractStatus::First,
                Ok(SnapshotOutcome::Unchanged { .. }) => ContractStatus::Unchanged,
                Err(e) => {
                    let mut notice = app.notice;
                    notice.set(Some(Notice::error(format!(
                        "Connected, but the snapshot was not saved: {e}"
                    ))));
                    // With no recorded baseline there is nothing to compare
                    // against, so reporting "unchanged" would be a lie.
                    ContractStatus::First
                }
            };
            let _ = app.store().mark_connected(id);

            let session = handle.clone();
            app.set_conn(
                id,
                Conn::Connected(Box::new(Connected {
                    handle,
                    snapshot,
                    status,
                    instructions: info.instructions.clone(),
                })),
            );
            app.reload_servers();
            // A change was possibly just recorded; the timeline should show it.
            app.reload_snapshots();
            app.watch_contract(id, session, listener);
        });
    }

    /// Re-snapshot whenever the server announces its lists changed, for as
    /// long as the session lives.
    ///
    /// This is the product's own headline scenario happening live — a contract
    /// change with the app watching must not wait for a manual reconnect.
    fn watch_contract(
        &self,
        id: ServerId,
        session: Handle,
        mut events: tokio::sync::broadcast::Receiver<Event>,
    ) {
        let app = *self;
        spawn(async move {
            loop {
                // The receiver rides through `off_scheduler` and back so the
                // await never sits on the Dioxus scheduler.
                let (received, returned) = match off_scheduler(async move {
                    let received = events.recv().await;
                    (received, events)
                })
                .await
                {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                events = returned;

                // Everything lands in the console, including the events
                // that also drive behaviour below. The pane's job is to be the
                // complete account of the session; a filtered one would send
                // people back to a terminal for the frame it dropped.
                if let Ok(event) = &received {
                    app.push_console(id, ConsoleLine::from_event(event));
                }

                match received {
                    Ok(
                        Event::ToolListChanged
                        | Event::ResourceListChanged
                        | Event::PromptListChanged,
                    ) => app.refresh_contract(id),
                    // The transport died underneath the session. Without this,
                    // a green dot keeps promising a connection that is gone,
                    // and the user only learns when the next call fails.
                    Ok(Event::Disconnected { reason }) => {
                        // Only this session may report itself dead — a
                        // reconnect may already own the slot.
                        let current = app
                            .conn(id)
                            .connected()
                            .is_some_and(|c| c.handle.same_session(&session));
                        if current {
                            app.set_conn(id, Conn::Failed(format!("Connection lost: {reason}")));
                        }
                        return;
                    }
                    // Everything else is already in the console above;
                    // contract changes are what additionally cannot wait.
                    Ok(_) => {}
                    // Missed a few — one refresh covers whatever they said,
                    // and the console says so rather than showing a silent gap.
                    Err(RecvError::Lagged(dropped)) => {
                        app.push_console(
                            id,
                            ConsoleLine::notice(&format!(
                                "console fell behind - {dropped} event(s) dropped"
                            )),
                        );
                        app.refresh_contract(id);
                    }
                    // Sender dropped: the session is gone, and so is our job.
                    Err(RecvError::Closed) => return,
                }
            }
        });
    }

    /// Re-snapshot a live session and fold the result into its `Conn`.
    fn refresh_contract(&self, id: ServerId) {
        let app = *self;
        spawn(async move {
            let Some(connected) = app.conn(id).connected().cloned() else {
                return;
            };

            let snapshot = {
                let handle = connected.handle.clone();
                match off_scheduler(async move { handle.snapshot().await }).await {
                    Ok(Ok(snapshot)) => snapshot,
                    // The session is dying; its conn state will say so through
                    // the ordinary paths.
                    _ => return,
                }
            };

            let status = match app.store().record_snapshot(id, &snapshot) {
                Ok(SnapshotOutcome::Changed { diff, .. }) => ContractStatus::Changed(diff),
                Ok(SnapshotOutcome::First { .. }) => ContractStatus::First,
                // The lists were re-announced but nothing user-visible moved.
                // Keep what the connect concluded rather than overwriting a
                // real diff with "unchanged".
                Ok(SnapshotOutcome::Unchanged { .. }) => connected.status.clone(),
                Err(e) => {
                    let mut notice = app.notice;
                    notice.set(Some(Notice::error(format!(
                        "The server changed mid-session, but the snapshot was not saved: {e}"
                    ))));
                    connected.status.clone()
                }
            };

            // A disconnect may have raced the refresh; do not resurrect a
            // session the user just closed.
            if app.conn(id).connected().is_none() {
                return;
            }
            app.set_conn(
                id,
                Conn::Connected(Box::new(Connected {
                    handle: connected.handle,
                    snapshot,
                    status,
                    instructions: connected.instructions,
                })),
            );
            app.reload_servers();
            app.reload_snapshots();
        });
    }

    /// Run the OAuth flow for a server, then connect.
    ///
    /// The browser opens and this waits for the redirect, so the connection is
    /// parked in `Authorizing` rather than looking idle for however long the
    /// user spends on the consent screen.
    pub fn sign_in(&self, id: ServerId) {
        let app = *self;
        let challenge = match app.conn(id) {
            Conn::NeedsAuth { challenge } => challenge,
            _ => None,
        };

        spawn(async move {
            let Ok(row) = app.store().get_server(id) else {
                return;
            };
            let url = row
                .config
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if url.is_empty() {
                return app.set_conn(id, Conn::Failed("This server has no URL.".into()));
            }

            app.set_conn(id, Conn::Authorizing);
            let key = config::credential_key(id);

            // `sign_in` puts its own flow on the runtime (see its docs), so it
            // is the one action that does not need wrapping here.
            let outcome =
                mcpclient::sign_in(&url, &key, challenge.as_deref(), CLIENT_METADATA_URL).await;
            tracing::info!(ok = outcome.is_ok(), "sign-in returned to the UI");
            match outcome {
                // The credentials are in the keychain now, so the ordinary
                // connect path will find them.
                Ok(()) => app.connect(id),
                Err(e) => app.set_conn(id, Conn::Failed(e.to_string())),
            }
        });
    }

    /// Forget a server's stored credentials and drop the session.
    pub fn sign_out(&self, id: ServerId) {
        let app = *self;
        spawn(async move {
            let key = config::credential_key(id);
            let cleared = off_scheduler(async move { mcpclient::sign_out(&key).await }).await;
            if let Err(e) = cleared.map_err(mcpclient::Error::Connect).and_then(|r| r) {
                let mut notice = app.notice;
                notice.set(Some(Notice::error(format!(
                    "Could not clear stored credentials: {e}"
                ))));
            }
            app.disconnect(id);
        });
    }

    pub fn disconnect(&self, id: ServerId) {
        let app = *self;
        spawn(async move {
            if let Some(connected) = app.conn(id).connected() {
                let handle = connected.handle.clone();
                let _ = off_scheduler(async move { handle.shutdown().await }).await;
            }
            app.set_conn(id, Conn::Disconnected);
        });
    }

    pub fn save_draft(&self) {
        let Some(draft) = self.draft.read().clone() else {
            return;
        };
        let config = match draft.to_config() {
            Ok(config) => config,
            Err(message) => {
                let mut signal = self.draft;
                if let Some(d) = signal.write().as_mut() {
                    d.error = Some(message);
                }
                return;
            }
        };

        let name = draft.name.trim();
        let result = match draft.id {
            Some(id) => self.store().update_server(id, name, &config).map(|_| id),
            None => self.store().add_server(NewServer {
                name: name.to_string(),
                transport_kind: draft.kind.into(),
                config,
            }),
        };

        let mut draft_signal = self.draft;
        match result {
            Ok(id) => {
                draft_signal.set(None);
                self.reload_servers();
                self.select_server(id);
            }
            Err(e) => {
                if let Some(d) = draft_signal.write().as_mut() {
                    d.error = Some(e.to_string());
                }
            }
        }
    }

    /// Persist a batch read out of another client's config file.
    ///
    /// An entry already in the library — same name, same transport config — is
    /// skipped rather than added again, so re-importing the file a user edits
    /// weekly picks up what is new instead of doubling what is not. Anything
    /// that differs is a genuinely different server and gets its own row, even
    /// when the name collides; two clients run `filesystem` against different
    /// directories all the time.
    ///
    /// Returns `(added, skipped)`.
    pub fn import_servers(&self, found: Vec<import::Imported>) -> (usize, usize) {
        let existing = self.servers.peek().clone();
        let store = self.store();
        let (mut added, mut skipped) = (0, 0);
        let mut last = None;

        for server in found {
            let duplicate = existing
                .iter()
                .any(|row| row.name == server.name && row.config == server.config);
            if duplicate {
                skipped += 1;
                continue;
            }
            match store.add_server(NewServer {
                name: server.name,
                transport_kind: server.kind,
                config: server.config,
            }) {
                Ok(id) => {
                    added += 1;
                    last = Some(id);
                }
                Err(e) => {
                    let mut notice = self.notice;
                    notice.set(Some(Notice::error(format!("Import failed: {e}"))));
                    break;
                }
            }
        }

        if added > 0 {
            self.reload_servers();
            if let Some(id) = last {
                self.select_server(id);
            }
        }
        (added, skipped)
    }

    /// Start a server's console over, seeded with whatever the handshake
    /// produced before anything could subscribe.
    fn reset_console(&self, id: ServerId, backlog: &[Event]) {
        let seeded: Vec<ConsoleLine> = backlog.iter().map(ConsoleLine::from_event).collect();
        let mut console = self.console;
        console.write().insert(id, seeded);
    }

    fn push_console(&self, id: ServerId, line: ConsoleLine) {
        let mut console = self.console;
        let mut console = console.write();
        let lines = console.entry(id).or_default();
        lines.push(line);
        // Trimmed from the front: the newest lines are the ones being read,
        // and a session that outlives the cap is one where the beginning has
        // already been scrolled past.
        if lines.len() > CONSOLE_LIMIT {
            lines.drain(..lines.len() - CONSOLE_LIMIT);
        }
    }

    /// Whether the selected server's session has said anything yet.
    ///
    /// Deliberately not a `console_lines() -> Vec<_>`: the buffer runs to a
    /// thousand entries with a JSON body on most of them, and a caller that
    /// only wants to know whether to offer a button should not clone it. The
    /// console pane reads the signal directly and clones only the lines it is
    /// about to render.
    pub fn console_has_lines(&self) -> bool {
        let Some(id) = *self.selected_server.read() else {
            return false;
        };
        self.console
            .read()
            .get(&id)
            .is_some_and(|lines| !lines.is_empty())
    }

    pub fn clear_console(&self) {
        let Some(id) = *self.selected_server.peek() else {
            return;
        };
        let mut console = self.console;
        console.write().insert(id, Vec::new());
    }

    pub fn delete_server(&self, id: ServerId) {
        let mut notice = self.notice;
        if let Err(e) = self.store().delete_server(id) {
            notice.set(Some(Notice::error(format!("Could not delete: {e}"))));
            return;
        }
        let mut conns = self.conns;
        conns.write().remove(&id);
        let mut console = self.console;
        console.write().remove(&id);

        let mut selected = self.selected_server;
        if *selected.read() == Some(id) {
            selected.set(None);
            let mut selection = self.selection;
            selection.set(None);
        }
        self.reload_servers();
    }
}

/// Await IO on the tokio runtime instead of on the Dioxus scheduler.
///
/// Dioxus polls the futures from `spawn` on its own executor, which is not
/// inside tokio's reactor context. A future that performs its own IO or arms a
/// `tokio::time` timer inline therefore makes no progress — *and* any timeout
/// guarding it never fires either, so the failure is a permanent silent hang
/// rather than an error. That is exactly how OAuth sign-in stalled on
/// "Waiting for the browser…" with nothing in the log.
///
/// It is not academic: rmcp's Streamable HTTP transport happens to run its IO
/// on a `WorkerTransport::spawn`'d task, so remote connects survived by luck,
/// while the stdio transport polls its pipes inline and `login_shell_path`
/// arms a timer on every stdio connect.
///
/// Signals are deliberately *not* written inside the spawned task: they belong
/// to the Dioxus runtime and must be touched from its own thread. The pattern
/// is always "await the IO here, then write the result on the Dioxus task".
async fn off_scheduler<F, T>(future: F) -> std::result::Result<T, String>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(future)
        .await
        .map_err(|e| format!("the background task did not finish: {e}"))
}

/// Dispatch one call against a live session.
///
/// The response is kept as raw JSON rather than a typed result so history rows,
/// the response pane, and the store all speak the same shape — and so a field
/// added by a future MCP revision survives the round trip.
pub(crate) async fn execute(
    handle: &Handle,
    selection: &Selection,
    request: &Value,
) -> Result<Value, mcpclient::Error> {
    let arguments = request.as_object().cloned().unwrap_or_default();

    let response = match selection.tab {
        Tab::Tools => serde_json::to_value(handle.call_tool(&selection.name, arguments).await?),
        Tab::Resources => serde_json::to_value(handle.read_resource(&selection.name).await?),
        Tab::Prompts => {
            let arguments = (!arguments.is_empty()).then_some(arguments);
            serde_json::to_value(handle.get_prompt(&selection.name, arguments).await?)
        }
    };

    Ok(response.unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })))
}

fn kind_of(tab: Tab) -> CallKind {
    match tab {
        Tab::Tools => CallKind::Tool,
        Tab::Resources => CallKind::Resource,
        Tab::Prompts => CallKind::Prompt,
    }
}

fn tab_of(kind: &CallKind) -> Tab {
    match kind {
        CallKind::Tool => Tab::Tools,
        CallKind::Resource => Tab::Resources,
        CallKind::Prompt => Tab::Prompts,
    }
}

impl AppState {
    pub fn reload_collections(&self) {
        let mut collections = self.collections;
        let Some(id) = *self.selected_server.read() else {
            collections.set(Vec::new());
            return;
        };
        match self.store().collections(id) {
            Ok(rows) => collections.set(rows),
            Err(e) => {
                let mut notice = self.notice;
                notice.set(Some(Notice::error(format!(
                    "Could not read collections: {e}"
                ))));
            }
        }
    }

    pub fn create_collection(&self, name: &str) {
        let Some(id) = *self.selected_server.read() else {
            return;
        };
        let name = name.trim();
        let name = if name.is_empty() { "Smoke test" } else { name };

        match self.store().create_collection(id, name) {
            Ok(collection) => {
                self.reload_collections();
                self.open_collection(collection);
            }
            Err(e) => {
                let mut notice = self.notice;
                notice.set(Some(Notice::error(format!(
                    "Could not create the collection: {e}"
                ))));
            }
        }
    }

    pub fn open_collection(&self, id: CollectionId) {
        let mut open = self.open_collection;
        let mut results = self.run_results;
        open.set(Some(id));
        // Results belong to a run, not to a collection; showing the previous
        // collection's outcomes against these steps would be nonsense.
        results.set(Vec::new());
        self.reload_steps();
    }

    pub fn close_collection(&self) {
        let mut open = self.open_collection;
        open.set(None);
    }

    fn reload_steps(&self) {
        let mut steps = self.steps;
        let Some(id) = *self.open_collection.read() else {
            steps.set(Vec::new());
            return;
        };
        steps.set(self.store().steps(id).unwrap_or_default());
    }

    /// Diagnose the URL currently in the dialog.
    ///
    /// The probe is handed to `tokio::spawn` rather than awaited inline. Dioxus
    /// polls this future on its own scheduler, outside tokio's reactor context,
    /// and `probe` does its HTTP directly in the awaited future — inline it
    /// would make no progress and no timeout would fire, which is exactly how
    /// OAuth sign-in hung silently before the same fix. Anything reached from a
    /// UI handler that does its own IO needs this.
    pub fn check_endpoint(&self) {
        let app = *self;
        let Some(url) = self
            .draft
            .read()
            .as_ref()
            .map(|d| d.url.trim().to_string())
            .filter(|u| !u.is_empty())
        else {
            return;
        };

        spawn(async move {
            let mut probing = app.probing;
            let mut report = app.endpoint_report;
            probing.set(true);
            report.set(None);

            let outcome = off_scheduler(async move { probe::probe(&url).await }).await;
            probing.set(false);
            match outcome {
                Ok(found) => report.set(Some(found)),
                Err(e) => {
                    let mut notice = app.notice;
                    notice.set(Some(Notice::error(e)));
                }
            }
        });
    }

    /// Replace the dialog's URL, used when the probe suggests a better one.
    pub fn use_suggested_url(&self, url: &str) {
        let mut draft = self.draft;
        if let Some(d) = draft.write().as_mut() {
            d.url = url.to_string();
            d.error = None;
        }
        let mut report = self.endpoint_report;
        report.set(None);
    }

    pub fn rename_collection(&self, id: CollectionId, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if self.store().rename_collection(id, name).is_ok() {
            self.reload_collections();
        }
    }

    pub fn delete_collection(&self, id: CollectionId) {
        if let Err(e) = self.store().delete_collection(id) {
            let mut notice = self.notice;
            notice.set(Some(Notice::error(format!(
                "Could not delete the collection: {e}"
            ))));
            return;
        }
        self.close_collection();
        self.reload_collections();
    }

    pub fn delete_step(&self, id: StepId) {
        let _ = self.store().delete_step(id);
        self.reload_steps();
    }

    /// Save whatever is currently in the call form as a step.
    ///
    /// Saving the form rather than the last result is deliberate: you build a
    /// smoke test out of the calls you meant to make, not the ones that
    /// happened to succeed.
    pub fn add_current_to_collection(&self, collection_id: CollectionId) {
        let Some(selection) = self.selection.read().clone() else {
            return;
        };
        let request = self.form.read().clone();

        let added = self.store().add_step(
            collection_id,
            kind_of(selection.tab),
            &selection.name,
            &request,
        );
        let mut notice = self.notice;
        match added {
            Ok(_) => {
                if *self.open_collection.read() == Some(collection_id) {
                    self.reload_steps();
                }
                notice.set(Some(Notice::info(format!(
                    "Added {} to the collection.",
                    selection.name
                ))));
            }
            Err(e) => notice.set(Some(Notice::error(format!("Could not add the step: {e}")))),
        }
    }

    /// Run every step in order against the live session.
    ///
    /// Sequential rather than concurrent: a smoke test's later steps routinely
    /// depend on what earlier ones did, and firing them all at once would make
    /// the result depend on scheduling.
    pub fn run_collection(&self, collection_id: CollectionId) {
        let app = *self;
        let Some(server_id) = *self.selected_server.read() else {
            return;
        };
        let Some(connected) = self.active() else {
            let mut notice = self.notice;
            notice.set(Some(Notice::error(
                "Connect to the server before running a collection.",
            )));
            return;
        };
        let steps = self.store().steps(collection_id).unwrap_or_default();

        spawn(async move {
            let mut running = app.running_collection;
            let mut results = app.run_results;
            running.set(true);
            results.set(Vec::new());

            for step in steps {
                let selection = Selection {
                    tab: tab_of(&step.kind),
                    name: step.target.clone(),
                };
                let started = std::time::Instant::now();
                let outcome = {
                    let (handle, selection, request) = (
                        connected.handle.clone(),
                        selection.clone(),
                        step.request.clone(),
                    );
                    match off_scheduler(async move { execute(&handle, &selection, &request).await })
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(e) => Err(mcpclient::Error::Connect(e)),
                    }
                };
                let duration_ms = started.elapsed().as_millis() as i64;

                let (response, is_error) = match outcome {
                    Ok(response) => {
                        let flagged = response
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        (response, flagged)
                    }
                    Err(e) => (serde_json::json!({ "error": e.to_string() }), true),
                };

                // A batch run is still a run: every step lands in history so it
                // can be opened and replayed on its own afterwards.
                let _ = app.store().record_call(NewCall {
                    server_id,
                    kind: step.kind.clone(),
                    target: step.target.clone(),
                    request: step.request.clone(),
                    response: response.clone(),
                    is_error,
                    duration_ms,
                });

                results.write().push(StepOutcome {
                    step_id: step.id,
                    target: step.target,
                    is_error,
                    duration_ms,
                    response,
                });
            }

            running.set(false);
            app.reload_history();
        });
    }
}
