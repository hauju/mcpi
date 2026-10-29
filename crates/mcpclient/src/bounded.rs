//! A Streamable HTTP client that stops reading a response past a byte cap.
//!
//! rmcp's own `reqwest::Client` backend reads a JSON reply, and the body of
//! every non-2xx answer, into memory whole before anything can look at its
//! size. On a hosted caller the server on the other end is whoever registered
//! the URL, so one endpoint answering with gigabytes takes the process down
//! for every user. This backend is rmcp 3.8's, with each body read capped:
//! a JSON reply, or an SSE response in total, past [`MAX_BODY`] is refused,
//! and an error body is cut at [`MAX_ERROR_BODY`] — it only feeds a message.
//!
//! The GET stream and DELETE are delegated unchanged: rmcp bounds each SSE
//! event, and a DELETE reads no body.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER, WWW_AUTHENTICATE,
};
use rmcp::model::{ClientJsonRpcMessage, ClientRequest, ErrorData, ServerJsonRpcMessage};
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::common::http_header::{
    EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION, HEADER_SESSION_ID,
    JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, HttpStatusError, InsufficientScopeError, StreamableHttpClient,
    StreamableHttpError, StreamableHttpPostResponse,
};

/// The largest JSON reply, or SSE response in total, read from one POST.
pub(crate) const MAX_BODY: usize = 16 * 1024 * 1024;

/// How much of an error body is kept for the message it ends up in.
const MAX_ERROR_BODY: usize = 64 * 1024;

type Error = StreamableHttpError<reqwest::Error>;

/// `reqwest::Client` with every response body read under a cap.
#[derive(Clone, Debug)]
pub(crate) struct Bounded(pub(crate) reqwest::Client);

impl StreamableHttpClient for Bounded {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, Error> {
        self.0
            .get_stream(uri, session_id, last_event_id, auth_token, custom_headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, Error> {
        self.0
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_token,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        self.0
            .delete_session(uri, session, auth_token, custom_headers)
            .await
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let mut request = self
            .0
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(auth_header) = auth_token {
            request = request.bearer_auth(auth_header);
        }
        for (name, value) in custom_headers {
            if is_reserved(&name) {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = request.json(&message).send().await?;
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let header = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::from(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_string();
            return Err(if status == StatusCode::UNAUTHORIZED {
                StreamableHttpError::AuthRequired(AuthRequiredError::new(header))
            } else {
                let scope = scope_of(&header);
                StreamableHttpError::InsufficientScope(InsufficientScopeError::new(header, scope))
            });
        }
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).to_string());
        let is_json = content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with(JSON_MIME_TYPE));
        // Rate limits and non-JSON failures return without reading the body;
        // a JSON server failure is read for the JSON-RPC error it may carry.
        let status_error = match (http_status_error(status, response.headers()), is_json) {
            (Some(error), is_json) if !is_json || status == StatusCode::TOO_MANY_REQUESTS => {
                return Err(error.into());
            }
            (error, _) => error,
        };
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // The spec wants 202 for these, but some servers send an empty 200.
        let no_reply_expected = matches!(
            message,
            ClientJsonRpcMessage::Notification(_)
                | ClientJsonRpcMessage::Response(_)
                | ClientJsonRpcMessage::Error(_)
        );
        if status.is_success() && response.content_length() == Some(0) && no_reply_expected {
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        if !status.is_success() {
            let body = read_capped(response, MAX_ERROR_BODY, Overflow::Truncate)
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_else(|_| "<failed to read response body>".to_owned());
            if let Some(response) =
                legacy_discover_response(&message, session_was_attached, status, &body)
            {
                return Ok(response);
            }
            // A 401/403 without a challenge keeps its status rather than pass
            // as an ordinary JSON-RPC error.
            let is_auth_failure =
                matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
            if !is_auth_failure && is_json {
                match serde_json::from_str::<ServerJsonRpcMessage>(&body) {
                    Ok(message @ ServerJsonRpcMessage::Error(_)) => {
                        return Ok(StreamableHttpPostResponse::Json(message, session_id));
                    }
                    _ => tracing::warn!(
                        "HTTP {status}: could not parse JSON body as a JSON-RPC error"
                    ),
                }
            }
            if let Some(error) = status_error {
                return Err(error.into());
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }

        match content_type.as_deref() {
            Some(ct) if ct.starts_with(EVENT_STREAM_MIME_TYPE) => {
                let mut read = 0usize;
                let bytes = response.bytes_stream().map(move |chunk| {
                    let chunk = chunk.map_err(std::io::Error::other)?;
                    read += chunk.len();
                    if read > MAX_BODY {
                        return Err(std::io::Error::other(too_large()));
                    }
                    Ok(chunk)
                });
                let events = sse_stream::SseStream::from_bytes_stream(bytes).boxed();
                Ok(StreamableHttpPostResponse::Sse(events, session_id))
            }
            Some(ct) if ct.starts_with(JSON_MIME_TYPE) => {
                let body = read_capped(response, MAX_BODY, Overflow::Refuse).await?;
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(parsed) => Ok(StreamableHttpPostResponse::Json(parsed, session_id)),
                    Err(e) if no_reply_expected => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as accepted: {e}"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                    Err(e) => Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                        format!(
                            "could not parse JSON response as ServerJsonRpcMessage: {e}: {}",
                            preview(&body)
                        ),
                    ))),
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Overflow {
    /// Keep the first `max` bytes and stop reading.
    Truncate,
    /// Fail the request.
    Refuse,
}

/// Read at most `max` bytes of the body, refusing a declared length past it
/// before reading any.
async fn read_capped(
    mut response: reqwest::Response,
    max: usize,
    overflow: Overflow,
) -> Result<Vec<u8>, Error> {
    let declared = response.content_length().unwrap_or(0);
    if overflow == Overflow::Refuse && declared > max as u64 {
        return Err(too_large());
    }
    let mut body = Vec::with_capacity((declared as usize).min(max));
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            if overflow == Overflow::Refuse {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk[..max - body.len()]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn too_large() -> Error {
    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!(
        "the server's response is larger than {} MiB",
        MAX_BODY / (1024 * 1024)
    )))
}

/// rmcp's reserved list: the worker sets these itself. `MCP-Protocol-Version`
/// is reserved but let through, because the worker injects it post-init.
fn is_reserved(name: &HeaderName) -> bool {
    [ACCEPT.as_str(), HEADER_SESSION_ID, HEADER_LAST_EVENT_ID]
        .iter()
        .any(|r| name.as_str().eq_ignore_ascii_case(r))
        && !name
            .as_str()
            .eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION)
}

/// The `scope=` parameter of a `WWW-Authenticate` value, quoted or not.
fn scope_of(header: &str) -> Option<String> {
    let start = header.to_ascii_lowercase().find("scope=")? + "scope=".len();
    let value = &header[start..];
    let scope = match value.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => value.split([',', ' ']).next().unwrap_or(value),
    };
    (!scope.is_empty()).then(|| scope.to_string())
}

/// HTTP 429 and 5xx as body-free metadata, as rmcp's backend reports them.
fn http_status_error(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Option<HttpStatusError> {
    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
        return None;
    }
    let retry_after = headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Some(HttpStatusError::new(status.as_u16(), retry_after))
}

/// A sessionless `server/discover` refused with a 4xx, re-correlated to the
/// request so the lifecycle can classify it — middleware rejections cannot
/// echo the id. rmcp keeps this private; it is copied as is.
fn legacy_discover_response(
    message: &ClientJsonRpcMessage,
    session_was_attached: bool,
    status: StatusCode,
    body: &str,
) -> Option<StreamableHttpPostResponse> {
    if session_was_attached
        || !status.is_client_error()
        || matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    {
        return None;
    }
    let ClientJsonRpcMessage::Request(request) = message else {
        return None;
    };
    if !matches!(request.request, ClientRequest::DiscoverRequest(_)) {
        return None;
    }
    let error = match serde_json::from_str::<ServerJsonRpcMessage>(body) {
        Ok(ServerJsonRpcMessage::Error(error)) => error.error,
        _ => ErrorData::invalid_request(
            format!("server/discover rejected with HTTP {status}: {body}"),
            None,
        ),
    };
    Some(StreamableHttpPostResponse::Json(
        ServerJsonRpcMessage::error(error, Some(request.id.clone())),
        None,
    ))
}

/// The first 256 bytes of a body that did not parse, cut on a char boundary.
fn preview(body: &[u8]) -> Cow<'_, str> {
    if body.is_empty() {
        return Cow::Borrowed("<empty>");
    }
    let lossy = String::from_utf8_lossy(body);
    match lossy.char_indices().nth(256) {
        Some((cut, _)) => Cow::Owned(lossy[..cut].to_owned()),
        None => lossy,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Serve one response: `head` (status line and headers, without the
    /// blank line), then `body_len` bytes of `fill` sent chunked, so no
    /// `Content-Length` gives the size away.
    async fn serve_once(head: &'static str, prefix: &'static [u8], body_len: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let header = format!("{head}\r\nTransfer-Encoding: chunked\r\n\r\n");
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            let mut chunk = prefix.to_vec();
            chunk.resize(64 * 1024, b' ');
            let mut sent = 0;
            while sent < body_len {
                let frame = format!("{:x}\r\n", chunk.len());
                let ok = socket.write_all(frame.as_bytes()).await.is_ok()
                    && socket.write_all(&chunk).await.is_ok()
                    && socket.write_all(b"\r\n").await.is_ok();
                if !ok {
                    return; // the client hung up, which is the point
                }
                sent += chunk.len();
                chunk.fill(b' ');
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        format!("http://{addr}/mcp")
    }

    fn ping() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "ping"
        }))
        .unwrap()
    }

    async fn post(url: String) -> Result<StreamableHttpPostResponse, Error> {
        Bounded(reqwest::Client::new())
            .post_message(url.into(), ping(), None, None, HashMap::new())
            .await
    }

    #[tokio::test]
    async fn a_json_reply_past_the_cap_is_refused() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"pad\":\"",
            MAX_BODY + 1024 * 1024,
        )
        .await;
        let Err(error) = post(url).await else {
            panic!("must be refused")
        };
        assert!(error.to_string().contains("larger than"), "{error}");
    }

    #[tokio::test]
    async fn a_small_json_reply_still_parses() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
            1,
        )
        .await;
        assert!(matches!(
            post(url).await,
            Ok(StreamableHttpPostResponse::Json(..))
        ));
    }

    #[tokio::test]
    async fn an_error_body_is_cut_not_buffered() {
        let url = serve_once(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain",
            b"nope",
            MAX_BODY * 2,
        )
        .await;
        let Err(error) = post(url).await else {
            panic!("a 400 is an error")
        };
        let message = error.to_string();
        assert!(message.contains("HTTP 400"), "{message}");
        assert!(message.len() <= MAX_ERROR_BODY + 100, "{}", message.len());
    }

    #[tokio::test]
    async fn an_sse_response_past_the_cap_ends_in_an_error() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream",
            b": keep-alive\n",
            MAX_BODY + 1024 * 1024,
        )
        .await;
        let Ok(StreamableHttpPostResponse::Sse(mut events, _)) = post(url).await else {
            panic!("expected an SSE response");
        };
        let mut errored = false;
        while let Some(event) = events.next().await {
            if event.is_err() {
                errored = true;
                break;
            }
        }
        assert!(errored, "the stream must fail once past the cap");
    }

    #[test]
    fn scope_reads_quoted_and_bare_values() {
        assert_eq!(
            scope_of(r#"Bearer error="insufficient_scope", scope="a b""#).as_deref(),
            Some("a b")
        );
        assert_eq!(
            scope_of("Bearer scope=read:data, x=y").as_deref(),
            Some("read:data")
        );
        assert_eq!(scope_of("Bearer realm=x"), None);
    }

    #[test]
    fn reserved_headers_match_rmcps_list() {
        for name in ["accept", "mcp-session-id", "last-event-id"] {
            assert!(is_reserved(&HeaderName::from_static(name)), "{name}");
        }
        for name in ["mcp-protocol-version", "x-api-key", "authorization"] {
            assert!(!is_reserved(&HeaderName::from_static(name)), "{name}");
        }
    }
}
