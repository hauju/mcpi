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
