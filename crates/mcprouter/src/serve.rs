//! Serving a [`Gateway`] over stdio or streamable HTTP.

use std::net::SocketAddr;

use axum::{Json, Router as AxumRouter, extract::State, routing::get};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::Gateway;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not start the MCP session: {0}")]
    Session(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Serve on this process's stdin/stdout until the client hangs up.
pub async fn serve_stdio(gateway: Gateway) -> Result<(), Error> {
    let service = gateway
        .serve(stdio())
        .await
        .map_err(|e| Error::Session(e.to_string()))?;
    service
        .waiting()
        .await
        .map_err(|e| Error::Session(e.to_string()))?;
    Ok(())
}

/// Serve streamable HTTP at `/mcp` plus a `/health` JSON endpoint, until Ctrl-C.
pub async fn serve_http(gateway: Gateway, bind: SocketAddr) -> Result<(), Error> {
    let ct = tokio_util::sync::CancellationToken::new();
    let mcp = StreamableHttpService::new(
        {
            let g = gateway.clone();
            move || Ok(g.clone())
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );
    let app = AxumRouter::new()
        .route("/health", get(health))
        .with_state(gateway)
        .nest_service("/mcp", mcp);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %bind, "listening (MCP at /mcp)");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}

async fn health(State(g): State<Gateway>) -> Json<serde_json::Value> {
    let mut servers: Vec<&str> = g.entries().iter().map(|e| e.server.as_str()).collect();
    servers.dedup();
    Json(serde_json::json!({
        "status": "ok",
        "tools": g.entries().len(),
        "upstreams": servers,
    }))
}
