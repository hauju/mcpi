//! The MCP server surface: `find_tools`, `call_tool`, plus any tools listed in `expose_direct`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData as McpError,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ResultType,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::log::{CatalogEntry, Record, RecordSink, now};
use crate::router::{Router, Routing, Verdict};
use crate::{ContractNote, Upstreams};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindToolsArgs {
    /// What the user wants to accomplish, in natural language. Include the concrete details
    /// (names, ids, paths) that decide which tool applies.
    pub request: String,
    /// Maximum number of tools to return (default 5).
    #[serde(default)]
    pub k: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallToolArgs {
    /// Tool name exactly as returned by `find_tools`.
    pub name: String,
    /// Arguments matching the tool's `inputSchema`.
    #[serde(default)]
    pub arguments: Option<JsonObject>,
}

#[derive(Serialize)]
struct FoundTool<'a> {
    name: &'a str,
    server: &'a str,
    description: Option<&'a str>,
    #[serde(rename = "inputSchema")]
    input_schema: &'a JsonObject,
    /// Router probability that this is the right tool.
    p: f64,
}

#[derive(Serialize)]
struct FindToolsResult<'a> {
    tools: Vec<FoundTool<'a>>,
    /// True when no tool stood out; the list is the best guess ranking.
    flat: bool,
    /// Whether the list is an answer, the nearest misses, or a request that
    /// wanted no tool in the first place.
    verdict: Verdict,
    /// That verdict in a sentence, for the model reading this. Absent when the
    /// ranking speaks for itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
    /// Upstreams whose contract changed since the gateway last saw them. Only present when
    /// one of the returned tools belongs to such an upstream.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    contract_changes: Vec<&'a ContractNote>,
}

/// What a `find_tools` in a session returned, so a following `call_tool` can be scored.
struct LastFind {
    find_id: u64,
    routing: Routing,
}

impl LastFind {
    /// Did this decision actually hand the model `name`?
    fn returned(&self, name: &str) -> bool {
        self.routing
            .candidates
            .iter()
            .position(|c| c.key == name)
            .is_some_and(|rank| self.routing.selected.contains(&rank))
    }
}

/// How many sessions keep decisions at all.
///
/// Nothing tells the gateway a session has ended — a client reconnecting with a
/// fresh `mcp-session-id` simply never returns — so the map is trimmed by age
/// instead: past this many sessions the one whose newest decision is oldest is
/// dropped. Each retained decision holds a `Scored` per catalogued tool, which
/// is the whole catalog while `shortlist` is 0.
const LINKED_SESSIONS: usize = 64;

/// How many `find_tools` decisions a session keeps for that scoring.
///
/// Only the newest used to be kept, which made the ledger lie about the router:
/// a model that searches twice and then calls both tools had its first call
/// scored against the second search, where that tool was never offered — a
/// rank-25 "miss" for a tool the router had in fact returned at rank 0. A
/// handful of decisions covers that interleaving; beyond it the call is old
/// enough that the newest decision is the fairer comparison anyway.
const LINKED_FINDS: usize = 8;

struct Inner {
    upstreams: Arc<dyn Upstreams>,
    router: Router,
    log: Option<Arc<dyn RecordSink>>,
    notes: Vec<ContractNote>,
    direct: Vec<Tool>,
    by_key: HashMap<String, usize>,
    find_seq: AtomicU64,
    last_find: Mutex<HashMap<String, VecDeque<LastFind>>>,
}

#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
    tool_router: ToolRouter<Gateway>,
}

/// Session key for linking `call_tool` to the preceding `find_tools`: the streamable HTTP
/// session id when present, otherwise the single stdio session.
fn session_key(ctx: &RequestContext<RoleServer>) -> String {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.headers.get("mcp-session-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stdio")
        .to_string()
}

fn def_chars(tool: &Tool) -> usize {
    serde_json::to_string(&serde_json::json!({
        "name": tool.name, "description": tool.description, "input_schema": tool.input_schema
    }))
    .map(|s| s.len())
    .unwrap_or(0)
}

#[tool_router]
impl Gateway {
    /// `notes` are the upstreams whose contract moved at connect time (from `schemadiff`).
    ///
    /// `log` is where routing decisions and proxied calls are recorded — a file for the CLI, a
    /// database for a hosted gateway, `None` to record nothing.
    pub fn new(
        upstreams: Arc<dyn Upstreams>,
        router: Router,
        log: Option<Arc<dyn RecordSink>>,
        notes: Vec<ContractNote>,
    ) -> Self {
        let by_key: HashMap<String, usize> = upstreams
            .entries()
            .iter()
            .enumerate()
            .map(|(i, e)| (e.key.clone(), i))
            .collect();
        let direct: Vec<Tool> = router
            .settings()
            .expose_direct
            .iter()
            .filter_map(|key| {
                let e = by_key.get(key).map(|&i| &upstreams.entries()[i]);
                if e.is_none() {
                    tracing::warn!(key, "expose_direct names a tool that no upstream provides");
                }
                let mut t = e?.tool.clone();
                t.name = e?.key.clone().into();
                Some(t)
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                upstreams,
                router,
                log,
                notes,
                direct,
                by_key,
                find_seq: AtomicU64::new(0),
                last_find: Mutex::new(HashMap::new()),
            }),
            tool_router: Self::tool_router(),
        }
    }

    pub fn entries(&self) -> &[crate::Entry] {
        self.inner.upstreams.entries()
    }

    fn entry(&self, key: &str) -> Option<&crate::Entry> {
        self.inner.by_key.get(key).map(|&i| &self.entries()[i])
    }

    /// Write the catalog snapshot to the JSONL log (once per start).
    pub async fn log_catalog(&self) {
        let Some(log) = &self.inner.log else { return };
        let tools: Vec<CatalogEntry<'_>> = self
            .entries()
            .iter()
            .map(|e| CatalogEntry {
                key: &e.key,
                server: &e.server,
                def_chars: def_chars(&e.tool),
            })
            .collect();
        log.write(&Record::Catalog {
            ts: now(),
            tools,
            expose_direct: &self.inner.router.settings().expose_direct,
            contract_changes: &self.inner.notes,
        })
        .await;
    }

    #[tool(
        name = "find_tools",
        description = "Find the upstream tools most likely needed for a request. Returns their names, descriptions and input schemas ranked by probability; call them with call_tool."
    )]
    pub async fn find_tools(
        &self,
        Parameters(args): Parameters<FindToolsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.find_tools_in(&session_key(&ctx), args).await
    }

    /// `find_tools` for a given session key (the MCP handler derives the key from the request).
    pub async fn find_tools_in(
        &self,
        session: &str,
        args: FindToolsArgs,
    ) -> Result<CallToolResult, McpError> {
        let inner = &self.inner;
        let routing = inner
            .router
            .route(self.entries(), &args.request, args.k)
            .await
            .map_err(|e| McpError::internal_error(format!("routing failed: {e}"), None))?;
        let find_id = inner.find_seq.fetch_add(1, Ordering::Relaxed) + 1;

        let tools: Vec<FoundTool<'_>> = routing
            .selected
            .iter()
            .filter_map(|&i| {
                let c = &routing.candidates[i];
                let e = self.entry(&c.key)?;
                Some(FoundTool {
                    name: &e.key,
                    server: &e.server,
                    description: e.tool.description.as_deref(),
                    input_schema: &e.tool.input_schema,
                    p: c.p,
                })
            })
            .collect();
        let contract_changes: Vec<&ContractNote> = inner
            .notes
            .iter()
            .filter(|n| tools.iter().any(|t| t.server == n.server))
            .collect();
        let returned: Vec<&str> = tools.iter().map(|t| t.name).collect();
        tracing::info!(
            request = %excerpt(&args.request),
            returned = ?returned,
            flat = routing.flat,
            latency_ms = routing.latency_ms,
            input_tokens = routing.input_tokens,
            "find_tools"
        );
        if let Some(log) = &inner.log {
            log.write(&Record::FindTools {
                ts: now(),
                session,
                find_id,
                request: &args.request,
                k: args.k.unwrap_or(inner.router.settings().k_default),
                routing: &routing,
                returned: returned.clone(),
            })
            .await;
        }
        let body = FindToolsResult {
            tools,
            flat: routing.flat,
            verdict: routing.verdict,
            note: routing.verdict.note(),
            contract_changes,
        };
        let json = serde_json::to_string(&body)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut finds = inner.last_find.lock().await;
        if finds.len() >= LINKED_SESSIONS && !finds.contains_key(session) {
            let coldest = finds
                .iter()
                .min_by_key(|(_, f)| f.back().map_or(0, |lf| lf.find_id))
                .map(|(key, _)| key.clone());
            if let Some(key) = coldest {
                finds.remove(&key);
            }
        }
        let session_finds = finds.entry(session.to_string()).or_default();
        if session_finds.len() == LINKED_FINDS {
            session_finds.pop_front();
        }
        session_finds.push_back(LastFind { find_id, routing });
        drop(finds);
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        name = "call_tool",
        description = "Call an upstream tool by the name returned from find_tools. The result is returned verbatim."
    )]
    pub async fn call_tool(
        &self,
        Parameters(args): Parameters<CallToolArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.call_tool_in(&session_key(&ctx), args).await
    }

    /// `call_tool` for a given session key.
    pub async fn call_tool_in(
        &self,
        session: &str,
        args: CallToolArgs,
    ) -> Result<CallToolResult, McpError> {
        self.proxy(&args.name, args.arguments, session, false).await
    }

    async fn proxy(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        session: &str,
        direct: bool,
    ) -> Result<CallToolResult, McpError> {
        let inner = &self.inner;
        let Some(entry) = self.entry(name) else {
            return Err(McpError::invalid_params(
                format!("unknown tool `{name}`; use find_tools to discover tools"),
                None,
            ));
        };
        let started = Instant::now();
        let result = inner.upstreams.call(name, arguments).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let (ok, is_error) = match &result {
            Ok(r) => (true, r.is_error.unwrap_or(false)),
            Err(_) => (false, true),
        };
        tracing::info!(name, direct, ok, is_error, latency_ms, "call_tool");
        if let Some(log) = &inner.log {
            let last = inner.last_find.lock().await;
            // Score the call against the most recent decision that actually
            // offered this tool, and only fall back to the newest one when no
            // decision did — that fallback is a real miss, worth recording.
            let linked = last
                .get(session)
                .and_then(|finds| {
                    finds
                        .iter()
                        .rev()
                        .find(|lf| lf.returned(name))
                        .or_else(|| finds.back())
                })
                .map(|lf| {
                    let rank = lf.routing.candidates.iter().position(|c| c.key == name);
                    (
                        lf.find_id,
                        rank,
                        rank.map(|r| lf.routing.candidates[r].p),
                        lf.returned(name),
                    )
                });
            // `linked` copies what it needs, and the write below is a database
            // round trip for a hosted sink — holding the lock across it would
            // serialise every find_tools and call_tool of the session behind it.
            drop(last);
            log.write(&Record::CallTool {
                ts: now(),
                session,
                name,
                server: Some(entry.server.as_str()),
                direct,
                find_id: linked.map(|l| l.0),
                rank: linked.and_then(|l| l.1),
                p: linked.and_then(|l| l.2),
                in_returned: linked.map(|l| l.3),
                ok,
                is_error,
                latency_ms,
            })
            .await;
        }
        match result {
            Ok(mut r) => {
                // The two sides of the gateway can sit on different protocol revisions.
                // An upstream older than `2026-07-28` sends no `resultType` (SEP-2322),
                // and rmcp only ever strips that discriminator for legacy peers — it
                // never adds it — so forwarding the upstream's result verbatim to a peer
                // that did negotiate `2026-07-28` produces a response the spec schema
                // rejects. Fill in the default the spec assigns to an absent field,
                // leaving an upstream that sent one untouched.
                r.result_type.get_or_insert(ResultType::COMPLETE);
                Ok(r)
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "upstream `{}` failed: {e}",
                entry.server
            ))])),
        }
    }
}

impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Gateway to several MCP servers. Call find_tools with a description of what you \
                 need; it returns the matching tools with their input schemas. Then call \
                 call_tool with that name and arguments. Tools listed here besides \
                 find_tools/call_tool can be called directly.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        tools.extend(self.inner.direct.iter().cloned());
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if self.inner.direct.iter().any(|t| t.name == request.name) {
            let name = request.name.to_string();
            return self
                .proxy(&name, request.arguments, &session_key(&ctx), true)
                .await
                .map(Into::into);
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, ctx))
            .await
    }
}

fn excerpt(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 120 {
        one.chars().take(119).collect::<String>() + "…"
    } else {
        one
    }
}
