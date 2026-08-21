//! OAuth for remote MCP servers, with credentials that survive a restart.
//!
//! The persistence is the point. The free inspector makes you re-authorize
//! every session because it holds tokens in a browser tab; here they go to the
//! system keychain, keyed per saved server, and a later connect picks them up
//! without a prompt.
//!
//! The flow follows RFC 8252 (OAuth for native apps): a loopback redirect on an
//! ephemeral port, PKCE, and the system browser rather than an embedded
//! webview. `rmcp` drives discovery, dynamic client registration, and the token
//! exchange; this module supplies the two things it cannot know about — where
//! to put the credentials, and how to catch the redirect.

use std::time::Duration;

use rmcp::transport::AuthError;
use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState, StoredCredentials,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::{Error, Result};

/// Keychain service for OAuth credentials. Distinct from the store's own
/// service name so a stale API key and a stale token cannot collide.
const SERVICE: &str = "mcpi-oauth";

/// How long the loopback listener waits for the browser to come back.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// How long discovery and client registration get before giving up.
///
/// Everything before the browser opens is machine-to-machine and should take
/// under a second. Without a bound, an authorization server that accepts the
/// connection and then says nothing leaves the app spinning on "Waiting for the
/// browser…" forever, with no browser and no way out.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Persists `rmcp`'s OAuth credentials in the system keychain.
///
/// Keyed per saved server: two servers behind the same authorization server
/// still get their own tokens, because they may have been authorized as
/// different users.
pub struct KeychainCredentials {
    account: String,
}

impl KeychainCredentials {
    pub fn new(credential_key: &str) -> Self {
        Self {
            account: credential_key.to_string(),
        }
    }

    fn entry(&self) -> std::result::Result<keyring::Entry, AuthError> {
        keyring::Entry::new(SERVICE, &self.account)
            .map_err(|e| AuthError::InternalError(format!("keychain unavailable: {e}")))
    }
}

// Keychain calls block and can put a system prompt on screen, so they are kept
// off the async runtime's threads.
#[async_trait::async_trait]
impl CredentialStore for KeychainCredentials {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        let entry = self.entry()?;
        let stored = tokio::task::spawn_blocking(move || entry.get_password())
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?;

        match stored {
            Ok(json) => serde_json::from_str(&json).map(Some).map_err(|e| {
                // A credential written by an older build is not worth failing a
                // connect over; treating it as absent re-runs sign-in.
                tracing::warn!("discarding unreadable stored credentials: {e}");
                AuthError::InternalError("stored credentials could not be read".into())
            }),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AuthError::InternalError(format!("keychain read: {e}"))),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let entry = self.entry()?;
        let json = serde_json::to_string(&credentials)
            .map_err(|e| AuthError::InternalError(format!("serialise credentials: {e}")))?;
        tokio::task::spawn_blocking(move || entry.set_password(&json))
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?
            .map_err(|e| AuthError::InternalError(format!("keychain write: {e}")))
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        let entry = self.entry()?;
        let result = tokio::task::spawn_blocking(move || entry.delete_credential())
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        match result {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AuthError::InternalError(format!("keychain delete: {e}"))),
        }
    }
}

/// Build a manager primed with whatever credentials are already stored.
///
/// Returns the manager either way: with no stored token the first request goes
/// out unauthenticated and the server's 401 drives sign-in, which is exactly
/// what `AuthClient` expects.
pub(crate) async fn manager_for(url: &str, credential_key: &str) -> Result<AuthorizationManager> {
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| Error::Connect(format!("could not prepare authorization: {e}")))?;
    manager.set_credential_store(KeychainCredentials::new(credential_key));

    // Failing to load is not fatal — it just means signing in again.
    if let Err(e) = manager.initialize_from_store().await {
        tracing::warn!("stored credentials unusable, continuing unauthenticated: {e}");
    }
    Ok(manager)
}

/// Run the interactive authorization flow to completion.
///
/// Blocks until the browser redirects back or [`CALLBACK_TIMEOUT`] elapses. On
/// success the credentials are in the keychain, and the next connect picks them
/// up with no further interaction.
///
/// `challenge` is the `WWW-Authenticate` header from the 401 that prompted
/// this; passing it lets discovery start from the resource metadata the server
/// pointed at rather than guessing.
///
/// `client_metadata_url` is a hosted client-ID metadata document (CIMD,
/// SEP-991): an HTTPS URL describing this client, used *as* the client id.
/// `rmcp` only uses it when the authorization server advertises
/// `client_id_metadata_document_supported`, falling back to dynamic client
/// registration otherwise — so passing it is always safe, and it is what
/// makes servers work that offer CIMD but no registration endpoint. `None`
/// keeps the registration-only behaviour.
///
/// The whole flow is handed to `tokio::spawn` rather than run inline. Callers
/// are UI event handlers, and Dioxus polls those futures on its own scheduler,
/// which is not inside tokio's reactor context. `rmcp`'s discovery performs its
/// HTTP request directly in the awaited future — with no IO driver it never
/// makes progress, and with no timer driver the timeout guarding it never fires
/// either, so the app hangs on "Waiting for the browser…" indefinitely. Moving
/// the work onto the runtime gives it both drivers; the caller is then only
/// awaiting a join handle, which needs neither.
pub async fn sign_in(
    url: &str,
    credential_key: &str,
    challenge: Option<&str>,
    client_metadata_url: Option<&str>,
) -> Result<()> {
    let (url, credential_key) = (url.to_string(), credential_key.to_string());
    let challenge = challenge.map(str::to_string);
    let client_metadata_url = client_metadata_url.map(str::to_string);

    tokio::spawn(async move {
        run_sign_in(
            &url,
            &credential_key,
            challenge.as_deref(),
            client_metadata_url.as_deref(),
        )
        .await
    })
    .await
    .map_err(|e| Error::Connect(format!("the sign-in task did not finish: {e}")))?
}

async fn run_sign_in(
    url: &str,
    credential_key: &str,
    challenge: Option<&str>,
    client_metadata_url: Option<&str>,
) -> Result<()> {
    // Bind first so the redirect URI names a port we are already listening on.
    // RFC 8252 §7.3 requires authorization servers to accept any port for a
    // loopback redirect, precisely so a native app can do this.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| Error::Connect(format!("could not open a local callback port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Connect(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    tracing::info!(
        %url,
        %redirect_uri,
        in_tokio_runtime = tokio::runtime::Handle::try_current().is_ok(),
        "starting sign-in"
    );

    // Everything up to the browser is bounded together: each step below can
    // hang on a server that never answers, and the user cannot tell them apart
    // from outside anyway.
    let mut oauth = tokio::time::timeout(DISCOVERY_TIMEOUT, async {
        tracing::debug!("resolving authorization server metadata");
        let mut oauth = OAuthState::new(url, None).await.map_err(|e| {
            Error::Connect(format!("could not reach the authorization server: {e}"))
        })?;

        if let OAuthState::Unauthorized(manager) = &mut oauth {
            manager.set_credential_store(KeychainCredentials::new(credential_key));
        }

        let mut request = AuthorizationRequest::new(redirect_uri).with_client_name("mcpi");
        if let Some(challenge) = challenge {
            tracing::debug!(%challenge, "using the server's challenge for discovery");
            request = request.with_challenge(challenge);
        }
        // rmcp prefers this over registration only when the authorization
        // server advertises CIMD support, so it is set unconditionally.
        if let Some(metadata_url) = client_metadata_url {
            request = request.with_client_metadata_url(metadata_url);
        }

        tracing::debug!("discovering and registering the client");
        oauth
            .start_authorization(request)
            .await
            .map_err(|e| Error::Connect(format!("authorization could not be started: {e}")))?;

        Ok::<_, Error>(oauth)
    })
    .await
    .map_err(|_| {
        Error::Connect(
            "the authorization server did not respond within 30s (discovery or client registration)"
                .into(),
        )
    })??;

    let authorization_url = oauth
        .get_authorization_url()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;

    tracing::info!(%authorization_url, "opening the browser");
    open_in_browser(&authorization_url).await?;
    tracing::info!("browser opened, waiting for the redirect");

    let callback = tokio::time::timeout(CALLBACK_TIMEOUT, await_callback(listener))
        .await
        .map_err(|_| Error::Connect("timed out waiting for the browser to come back".into()))??;

    tracing::debug!("redirect received, exchanging the code for a token");
    tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        oauth.handle_callback(&callback.code, &callback.state),
    )
    .await
    .map_err(|_| Error::Connect("the token exchange did not complete within 30s".into()))?
    .map_err(|e| Error::Connect(format!("the authorization code was rejected: {e}")))?;

    // `handle_callback` has already written the credentials through the
    // credential store, which is the only lasting effect this flow needs — the
    // next connect builds a fresh manager and loads them back. Advancing
    // rmcp's in-memory state machine past this point would only mutate an
    // object about to be dropped.
    tracing::info!("signed in; credentials stored");
    Ok(())
}

/// Forget a server's stored credentials.
pub async fn sign_out(credential_key: &str) -> Result<()> {
    KeychainCredentials::new(credential_key)
        .clear()
        .await
        .map_err(|e| Error::Connect(e.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
struct Callback {
    code: String,
    state: String,
}

/// Serve exactly one request, then close.
///
/// A full HTTP server would be several dependencies to read one query string.
/// The request line is all that matters and it arrives in the first packet.
async fn await_callback(listener: TcpListener) -> Result<Callback> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| Error::Connect(format!("callback connection failed: {e}")))?;

        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        BufReader::new(read)
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Connect(format!("could not read the callback: {e}")))?;

        let outcome = parse_callback(&line);
        let body = match &outcome {
            Some(Ok(_)) => "<h1>Signed in</h1><p>You can close this tab and return to mcpi.</p>",
            Some(Err(message)) => {
                tracing::warn!("authorization callback reported an error: {message}");
                "<h1>Authorization failed</h1><p>Return to MCP Inspector for details.</p>"
            }
            // Browsers routinely fetch /favicon.ico against the redirect host;
            // answering and waiting again avoids aborting the sign-in over it.
            None => "<h1>Waiting for authorization…</h1>",
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = write.write_all(response.as_bytes()).await;
        let _ = write.shutdown().await;

        match outcome {
            Some(Ok(callback)) => return Ok(callback),
            Some(Err(message)) => {
                return Err(Error::Connect(format!(
                    "authorization was refused: {message}"
                )));
            }
            None => continue,
        }
    }
}

/// Pull `code` and `state` out of a raw HTTP request line.
///
/// `None` means "not the callback" (keep listening); `Err` means the
/// authorization server reported a failure.
fn parse_callback(request_line: &str) -> Option<std::result::Result<Callback, String>> {
    let target = request_line.split_whitespace().nth(1)?;
    // The request line carries an origin-form target, which `Url` will only
    // parse against a base.
    let url = url::Url::parse("http://127.0.0.1")
        .ok()?
        .join(target)
        .ok()?;
    if url.path() != "/callback" {
        return None;
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        let message = match description {
            Some(description) => format!("{error}: {description}"),
            None => error,
        };
        return Some(Err(message));
    }

    match (code, state) {
        (Some(code), Some(state)) => Some(Ok(Callback { code, state })),
        // A /callback with neither is not something to keep waiting on.
        _ => Some(Err("the redirect carried no authorization code".into())),
    }
}

/// Hand the authorization URL to the system browser.
///
/// Waits for the opener and checks what it said. `spawn()` alone only reports
/// that the process started — `open` exiting non-zero because it could not
/// handle the URL would look identical to success, and the user would be left
/// staring at a spinner with no tab and no error.
async fn open_in_browser(url: &str) -> Result<()> {
    // Absolute paths: a bundled macOS app launched from Finder inherits a
    // minimal `PATH`, and this is not worth failing over a lookup.
    let opener = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };

    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(opener).arg(url).output(),
    )
    .await
    .map_err(|_| Error::Connect(format!("`{opener}` did not return")))?
    .map_err(|e| Error::Connect(format!("could not run `{opener}`: {e}")))?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(Error::Connect(if detail.is_empty() {
            format!("`{opener}` failed with {}", output.status)
        } else {
            format!("could not open a browser: {detail}")
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_redirect_yields_the_code_and_state() {
        let parsed = parse_callback("GET /callback?code=abc&state=xyz HTTP/1.1\r\n");
        assert_eq!(
            parsed,
            Some(Ok(Callback {
                code: "abc".into(),
                state: "xyz".into()
            }))
        );
    }

    #[test]
    fn percent_encoding_is_decoded() {
        let parsed = parse_callback("GET /callback?code=a%2Fb%2Bc&state=s HTTP/1.1\r\n");
        let Some(Ok(callback)) = parsed else {
            panic!("expected a callback, got {parsed:?}");
        };
        assert_eq!(callback.code, "a/b+c");
    }

    #[test]
    fn a_denied_authorization_reports_the_reason() {
        let parsed = parse_callback(
            "GET /callback?error=access_denied&error_description=User%20said%20no HTTP/1.1\r\n",
        );
        assert_eq!(parsed, Some(Err("access_denied: User said no".into())));
    }

    #[test]
    fn an_unrelated_request_is_ignored_rather_than_failing_the_flow() {
        // Browsers fetch this against the redirect host unprompted; aborting
        // sign-in over it would make the flow fail at random.
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1\r\n"), None);
    }

    #[test]
    fn a_callback_without_a_code_is_an_error_not_a_wait() {
        assert!(matches!(
            parse_callback("GET /callback HTTP/1.1\r\n"),
            Some(Err(_))
        ));
    }

    #[test]
    fn a_malformed_request_line_is_ignored() {
        assert_eq!(parse_callback("garbage\r\n"), None);
        assert_eq!(parse_callback(""), None);
    }
}
