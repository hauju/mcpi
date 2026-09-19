//! A remote server that demands authorization, answered by a real socket.
//!
//! The value here is the error path. `AuthRequiredError` arrives boxed inside a
//! transport error inside an initialize error, and the client finds it by
//! walking the source chain and downcasting. That is exactly the kind of wiring
//! that breaks silently on an `rmcp` upgrade — and when it does, the UI stops
//! offering a sign-in button and shows a generic failure instead.

use std::collections::BTreeMap;

use mcpclient::{Error, Handle, Transport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve `status` with the given headers to every request, forever.
async fn serve_fixed_response(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // Read whatever the client sent so it does not see a reset
                // before the response lands.
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    format!("http://127.0.0.1:{port}")
}

fn remote(url: String) -> Transport {
    Transport::Http {
        url: format!("{url}/mcp"),
        headers: BTreeMap::new(),
        // `None` keeps the keychain out of the test: a real account would
        // prompt on a developer machine and has nowhere to live in CI.
        credential_key: None,
    }
}

#[tokio::test]
async fn a_401_becomes_an_offer_to_sign_in_rather_than_a_failure() {
    const UNAUTHORIZED: &str = concat!(
        "HTTP/1.1 401 Unauthorized\r\n",
        "WWW-Authenticate: Bearer realm=\"mcp\", ",
        "resource_metadata=\"http://127.0.0.1/.well-known/oauth-protected-resource\"\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n\r\n"
    );

    let base = serve_fixed_response(UNAUTHORIZED).await;
    let error = Handle::connect(&remote(base))
        .await
        .expect_err("a 401 cannot complete a handshake");

    match error {
        Error::AuthRequired { challenge } => {
            let challenge = challenge.expect("the challenge drives OAuth discovery");
            assert!(
                challenge.contains("resource_metadata"),
                "the WWW-Authenticate header must survive intact: {challenge}"
            );
        }
        other => {
            panic!("a 401 must surface as AuthRequired so the UI can offer sign-in, got: {other:?}")
        }
    }
}

#[tokio::test]
async fn a_server_error_that_is_not_a_401_stays_a_plain_failure() {
    // The classifier walks the whole source chain; this is the test that it
    // does not decide everything is an auth problem.
    const BROKEN: &str = concat!(
        "HTTP/1.1 500 Internal Server Error\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n\r\n"
    );

    let base = serve_fixed_response(BROKEN).await;
    let error = Handle::connect(&remote(base))
        .await
        .expect_err("a 500 cannot complete a handshake");

    assert!(
        !matches!(error, Error::AuthRequired { .. }),
        "a 500 is not a sign-in prompt: {error:?}"
    );
}

#[tokio::test]
async fn a_deprecated_sse_endpoint_names_its_own_problem() {
    // Servers on the old two-endpoint transport are common enough that a
    // generic connection error would send people hunting for the wrong bug.
    const NOT_FOUND: &str = concat!(
        "HTTP/1.1 404 Not Found\r\n",
        "Content-Length: 0\r\n",
        "Connection: close\r\n\r\n"
    );

    let base = serve_fixed_response(NOT_FOUND).await;
    let transport = Transport::Http {
        url: format!("{base}/sse"),
        headers: BTreeMap::new(),
        credential_key: None,
    };

    let error = Handle::connect(&transport)
        .await
        .expect_err("the legacy endpoint cannot speak Streamable HTTP");

    assert!(
        matches!(error, Error::LegacySseTransport),
        "expected the transport to be named, got: {error:?}"
    );
    assert!(
        error.to_string().contains("deprecated"),
        "the message has to say why: {error}"
    );
}

/// Capture the first request line-block each connection sends, so a test can
/// assert what actually went on the wire.
async fn capture_requests(
    response: &'static str,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                if let Ok(n) = stream.read(&mut buffer).await {
                    sink.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buffer[..n]).to_string());
                }
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (format!("http://127.0.0.1:{port}"), seen)
}

const UNAUTHORIZED_PLAIN: &str = concat!(
    "HTTP/1.1 401 Unauthorized\r\n",
    "WWW-Authenticate: Bearer realm=\"mcp\"\r\n",
    "Content-Length: 0\r\n",
    "Connection: close\r\n\r\n"
);

/// The header plumbing is shared by every HTTP dial, public or not. It is
/// exercised through the unrestricted transport because the public one refuses
/// loopback by design — and refusing loopback is the next test.
#[tokio::test]
async fn static_headers_reach_the_server() {
    let (base, seen) = capture_requests(UNAUTHORIZED_PLAIN).await;
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer upstream-secret".to_string(),
    );
    headers.insert("X-Tenant".to_string(), "acme".to_string());

    let _ = Handle::connect(&Transport::Http {
        url: format!("{base}/mcp"),
        headers,
        credential_key: None,
    })
    .await;

    let requests = seen.lock().unwrap();
    let sent = requests
        .first()
        .expect("the client must have sent a request");
    assert!(
        sent.contains("authorization: Bearer upstream-secret"),
        "the bearer must ride the request: {sent}"
    );
    assert!(sent.contains("x-tenant: acme"), "{sent}");
}

#[tokio::test]
async fn a_header_that_is_not_a_header_is_refused_before_any_connection() {
    let mut headers = BTreeMap::new();
    headers.insert("not a header name".to_string(), "x".to_string());
    let error = Handle::connect_public_with_headers("https://example.com/mcp", &headers)
        .await
        .expect_err("an unusable header name cannot be sent");
    assert!(
        error.to_string().contains("not a valid header name"),
        "{error}"
    );
}

/// The whole reason this dial exists: a hosted caller must not be talked into
/// connecting to its own network, and the check happens before any socket.
#[tokio::test]
async fn the_public_dial_refuses_private_destinations() {
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), "Bearer secret".to_string());

    // A real loopback listener, so a dial that was *not* refused would succeed
    // rather than merely fail to connect.
    let (base, seen) = capture_requests(UNAUTHORIZED_PLAIN).await;
    assert!(
        Handle::connect_public_with_headers(&format!("{base}/mcp"), &headers)
            .await
            .is_err(),
        "loopback is not a public address"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "the credential must never leave the process for a private destination"
    );

    for url in [
        "http://10.0.0.1/mcp",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/mcp",
        "file:///etc/hosts",
    ] {
        assert!(
            Handle::connect_public_with_headers(url, &headers)
                .await
                .is_err(),
            "{url} must be refused"
        );
    }
}
