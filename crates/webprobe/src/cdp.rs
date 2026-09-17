//! Just enough of the Chrome DevTools Protocol to read a page.
//!
//! Four commands are needed in total — create a target, attach to it, navigate,
//! evaluate — so this is a request/response loop over a WebSocket rather than a
//! CDP client. Nothing here subscribes to a domain (`Page.enable`,
//! `Runtime.enable`), which is what keeps it this small: with no event streams
//! turned on, almost every frame that arrives is the reply we are waiting for,
//! and the few that are not can simply be skipped.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::{Error, Result};

/// A single command is allowed this long before the connection is assumed bad.
///
/// This is not the page's budget — [`crate::Scan::timeout`] covers that. It
/// only catches a Chrome that has stopped answering at all.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Cdp {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl Cdp {
    pub(crate) async fn connect(ws_url: &str) -> Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| Error::Cdp(format!("could not open {ws_url}: {e}")))?;
        Ok(Self { socket, next_id: 0 })
    }

    /// Send one command and wait for the reply with the matching id.
    ///
    /// `session` selects a page when set; `None` addresses the browser itself.
    pub(crate) async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;

        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }

        self.socket
            .send(Message::text(request.to_string()))
            .await
            .map_err(|e| Error::Cdp(format!("sending {method} failed: {e}")))?;

        tokio::time::timeout(CALL_TIMEOUT, self.reply(id, method))
            .await
            .map_err(|_| Error::Cdp(format!("{method} did not answer within 10s")))?
    }

    async fn reply(&mut self, id: u64, method: &str) -> Result<Value> {
        loop {
            let frame = self
                .socket
                .next()
                .await
                .ok_or_else(|| Error::Cdp("Chrome closed the connection".into()))?
                .map_err(|e| Error::Cdp(e.to_string()))?;

            let Message::Text(text) = frame else {
                // Chrome speaks JSON over text frames; pings and the like are
                // handled by the library, and anything else is not for us.
                continue;
            };

            let Ok(message) = serde_json::from_str::<Value>(&text) else {
                continue;
            };

            // An event carries no `id`. We enable no domains, so these are rare
            // and never something this loop is waiting for.
            if message["id"].as_u64() != Some(id) {
                continue;
            }

            if let Some(error) = message.get("error") {
                let text = error["message"].as_str().unwrap_or("unknown error");
                return Err(Error::Cdp(format!("{method}: {text}")));
            }

            return Ok(message["result"].clone());
        }
    }
}
