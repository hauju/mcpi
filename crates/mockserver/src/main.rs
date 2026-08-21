//! A stdio MCP server that exists to be diffed against itself.
//!
//! `--variant a` and `--variant b` advertise deliberately different contracts,
//! chosen so that between them they exercise every row of `schemadiff`'s
//! severity table: a tool removed and one added, a required property added, a
//! bound tightened, an enum value dropped, an optional property added, a
//! resource changing MIME type, a prompt gaining a required argument, and a
//! description edit that must stay cosmetic. One tool (`echo`) is identical in
//! both, so a diff that reports it is wrong.
//!
//! Schemas are written by hand rather than derived from Rust types: the whole
//! point of the fixture is precise control over what `tools/list` emits, and
//! `#[tool]` would derive one fixed schema per type.
//!
//! Run it directly to sanity-check output:
//! `echo '{"jsonrpc":"2.0","id":1,"method":"initialize",...}' | cargo run -p mockserver`

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, JsonObject, ListPromptsResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument,
    PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServiceExt};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    A,
    B,
}

impl Variant {
    fn from_args() -> Self {
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let value = match arg.as_str() {
                "--variant" => args.next(),
                other => other.strip_prefix("--variant=").map(str::to_string),
            };
            match value.as_deref() {
                Some("a") | Some("A") => return Self::A,
                Some("b") | Some("B") => return Self::B,
                Some(other) => {
                    eprintln!("unknown variant `{other}`; expected `a` or `b`");
                    std::process::exit(2);
                }
                None => {}
            }
        }
        Self::A
    }
}

#[derive(Debug, Clone)]
struct Mock {
    variant: Variant,
}

/// `serde_json::Map`, which is what rmcp wants for a raw schema.
fn schema(value: Value) -> Arc<JsonObject> {
    Arc::new(
        value
            .as_object()
            .expect("schemas in this fixture are always objects")
            .clone(),
    )
}

impl Mock {
    fn tools(&self) -> Vec<Tool> {
        let mut tools = vec![self.search_tool(), echo_tool()];
        match self.variant {
            // Removed in B → breaking.
            Variant::A => tools.push(Tool::new(
                "deprecated_tool",
                "Scheduled for removal.",
                schema(json!({ "type": "object", "properties": {} })),
            )),
            // Added in B → compatible.
            Variant::B => tools.push(Tool::new(
                "summarize",
                "Summarise a document.",
                schema(json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                })),
            )),
        }
        tools
    }

    fn search_tool(&self) -> Tool {
        match self.variant {
            Variant::A => Tool::new(
                "search",
                "Search the index.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "What to look for" },
                        "limit": { "type": "integer", "maximum": 100, "minimum": 1 },
                        "mode":  { "type": "string", "enum": ["fast", "slow", "auto"] },
                    },
                    "required": ["query"],
                })),
            ),
            // Every edit below is deliberate:
            //   description   → cosmetic
            //   index         → required property added, breaking
            //   cursor        → optional property added, compatible
            //   limit.maximum → tightened, breaking
            //   mode.enum     → `auto` dropped, breaking
            Variant::B => Tool::new(
                "search",
                "Search the index. Now with pagination.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query":  { "type": "string", "description": "What to look for" },
                        "index":  { "type": "string", "description": "Which index to search" },
                        "cursor": { "type": "string", "description": "Opaque page cursor" },
                        "limit":  { "type": "integer", "maximum": 10, "minimum": 1 },
                        "mode":   { "type": "string", "enum": ["fast", "slow"] },
                    },
                    "required": ["query", "index"],
                })),
            ),
        }
    }

    fn resources(&self) -> Vec<Resource> {
        let mime = match self.variant {
            Variant::A => "text/markdown",
            // Breaking: consumers parse the body based on this.
            Variant::B => "text/html",
        };
        vec![
            Resource::new("mock://notes", "notes")
                .with_mime_type(mime)
                .with_description("A fixture document."),
        ]
    }

    fn prompts(&self) -> Vec<Prompt> {
        let mut arguments = vec![
            PromptArgument::new("style")
                .with_description("Tone to use.")
                .with_required(false),
        ];
        if self.variant == Variant::B {
            // Breaking: existing callers do not send it.
            arguments.push(
                PromptArgument::new("path")
                    .with_description("File to review.")
                    .with_required(true),
            );
        }
        vec![Prompt::new(
            "review",
            Some("Review a change."),
            Some(arguments),
        )]
    }
}

fn echo_tool() -> Tool {
    Tool::new(
        "echo",
        "Echo the message back.",
        schema(json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
        })),
    )
}

impl ServerHandler for Mock {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("mockserver", env!("CARGO_PKG_VERSION")))
        .with_instructions("Fixture server. Variants `a` and `b` differ on purpose.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // An unadvertised escape hatch for liveness tests: exit without
        // replying, the way a crashing server dies mid-session. Deliberately
        // absent from `tools/list` so contract assertions never see it.
        if request.name.as_ref() == "die" {
            std::process::exit(0);
        }

        // Only serve what this variant actually advertises. Answering a tool
        // that is absent from `tools/list` would make the fixture lie about its
        // own contract — and a caller invoking a removed tool has to see it
        // fail, which is the whole point of removing it.
        if !self.tools().iter().any(|t| t.name == request.name) {
            return Err(McpError::invalid_params(
                format!("unknown tool `{}`", request.name),
                None,
            ));
        }

        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = match request.name.as_ref() {
            "echo" => {
                let message = args
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| McpError::invalid_params("`message` is required", None))?;
                CallToolResult::success(vec![ContentBlock::text(message)])
            }
            // Echoing the arguments back makes the response pane easy to assert
            // on, from tests and by eye.
            "search" | "summarize" | "deprecated_tool" => {
                CallToolResult::success(vec![ContentBlock::text(format!(
                    "{}: {args}",
                    request.name
                ))])
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool `{other}`"),
                    None,
                ));
            }
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(self.resources()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if request.uri != "mock://notes" {
            return Err(McpError::resource_not_found(
                format!("no resource at `{}`", request.uri),
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            "# Notes\n\nFixture body.",
            &request.uri,
        )])
        .into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult::with_all_items(self.prompts()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        if request.name != "review" {
            return Err(McpError::invalid_params(
                format!("unknown prompt `{}`", request.name),
                None,
            ));
        }
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            "Please review the change.",
        )])
        .with_description("Review a change.")
        .into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let variant = Variant::from_args();
    // stdout is the transport, so anything diagnostic has to go to stderr.
    eprintln!("mockserver: serving variant {variant:?} over stdio");

    let service = Mock { variant }.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
