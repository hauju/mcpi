//! Reads a page's WebMCP tool contract.
//!
//! A WebMCP page registers its tools with the browser rather than answering an
//! HTTP request, so there is nothing to probe from the outside: the tools only
//! exist inside a tab that has run the page's JavaScript. This crate drives a
//! real Chrome over the DevTools Protocol, waits for the tool set to stop
//! moving, and hands back a [`schemadiff::Snapshot`] in the same shape the MCP
//! probe records — so history and classification work unchanged.
//!
//! The one rule everything here is built around:
//!
//! > **An empty tool list is never quietly a snapshot.**
//!
//! Tools register after load, sometimes after a sign-in, sometimes per route.
//! Reading too early sees nothing, and "nothing" diffed against a baseline
//! reads as *every tool removed* — a breaking verdict for a page that did not
//! change. So a scan reports what it actually established ([`Outcome`]) and
//! only one of those four answers is a snapshot.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use schemadiff::Snapshot;
use serde::{Deserialize, Serialize};

mod cdp;
mod chrome;
mod snapshot;
#[cfg(test)]
mod tests;

/// How often the tool set is re-read while waiting for it to settle.
const POLL: Duration = Duration::from_millis(250);

/// Evaluated in the page on every poll.
///
/// Returns the three things a decision needs — where the tab actually is, how
/// far it has loaded, and the tool set — in one round trip, so they cannot
/// disagree with each other across calls.
const PROBE_JS: &str = r#"
(async () => {
  const out = {
    href: location.href,
    ready: document.readyState,
    supported: typeof document.modelContext !== 'undefined'
      && document.modelContext !== null,
    tools: null,
    error: null,
  };
  if (!out.supported || out.ready !== 'complete') return out;
  try {
    const tools = await document.modelContext.getTools();
    out.tools = tools.map((tool) => {
      // Flattened here rather than by `returnByValue`, which walks the real
      // descriptors into their internal graph and gives up with "Object
      // reference chain is too long".
      //
      // `for...in` rather than Object.keys: a browser-provided descriptor puts
      // its fields on the prototype, where own-key enumeration finds nothing.
      // Copying whatever is enumerable — instead of a fixed list of fields —
      // is what keeps a property added by a future WebMCP revision visible in
      // a diff instead of silently dropped.
      const plain = {};
      for (const key in tool) {
        const value = tool[key];
        if (typeof value === 'function') continue;
        try {
          plain[key] = JSON.parse(JSON.stringify(value));
        } catch (e) {
          // Not representable, so not comparable either. Recorded as a marker
          // rather than omitted, so the field's presence still shows up.
          plain[key] = '[unserializable]';
        }
      }
      return plain;
    });
  } catch (e) {
    out.error = String((e && e.message) || e);
  }
  return out;
})()
"#;

/// Which Chrome the scan runs in.
///
/// What a scan can see is decided entirely by the *profile* it runs in, not by
/// which variant is used: a profile that has signed in sees the signed-in tool
/// set, and one that has not sees the anonymous one. Attaching to the user's
/// everyday browser is not among the options, because Chrome has refused
/// `--remote-debugging-port` on its default profile since version 136.
#[derive(Debug, Clone)]
pub enum Browser {
    /// Attach to a DevTools endpoint somebody else is already running.
    ///
    /// Always a purpose-started browser on some non-default profile, for the
    /// reason above. Kept for people who run one deliberately; it is not how
    /// the app reaches a page.
    Attached {
        /// The DevTools HTTP endpoint, e.g. `http://127.0.0.1:9222`.
        debug_url: String,
    },
    /// Launch a Chrome this process owns for the length of the scan.
    Launched {
        /// Explicit binary, or `None` to search the usual places.
        binary: Option<PathBuf>,
        /// A profile that persists between scans, so a sign-in done once is
        /// still there next time. `None` uses a throwaway directory, which is
        /// signed out by construction — the CI shape.
        profile: Option<PathBuf>,
        /// `false` opens a real window, which is the only way a person can
        /// complete a sign-in.
        headless: bool,
    },
}

impl Browser {
    /// A headless scan in a profile that keeps what it learns.
    pub fn profile(path: impl Into<PathBuf>) -> Self {
        Self::Launched {
            binary: None,
            profile: Some(path.into()),
            headless: true,
        }
    }

    /// A headless scan in a throwaway profile: signed out, every time.
    pub fn anonymous() -> Self {
        Self::Launched {
            binary: None,
            profile: None,
            headless: true,
        }
    }

    /// The same profile, with a window, so somebody can sign in.
    pub fn visible(self) -> Self {
        match self {
            Self::Launched {
                binary, profile, ..
            } => Self::Launched {
                binary,
                profile,
                headless: false,
            },
            attached => attached,
        }
    }
}

/// One scan of one page.
#[derive(Debug, Clone)]
pub struct Scan {
    pub url: String,
    pub browser: Browser,
    /// How long the tool set must stay identical before it counts as settled.
    pub settle: Duration,
    /// Upper bound on the whole wait, settle window included.
    pub timeout: Duration,
}

impl Scan {
    /// A scan of `url` with the defaults: two seconds of quiet, thirty overall.
    ///
    /// The settle window is deliberately generous. Being slow is a cost; being
    /// wrong about a contract is the bug this project exists to not have.
    pub fn new(url: impl Into<String>, browser: Browser) -> Self {
        Self {
            url: url.into(),
            browser,
            settle: Duration::from_secs(2),
            timeout: Duration::from_secs(30),
        }
    }
}

/// What the browser held for the page's origin when the scan ran.
///
/// **Names and expiries only, never values.** Enough to answer "is the session
/// that was here last time still here", and useless to anyone who reads it —
/// which is what lets it be stored beside the contract rather than treated as
/// a credential.
///
/// A scan cannot tell from the page whether it is signed in: a signed-out page
/// looks like a signed-in one with fewer tools, and fewer tools is exactly what
/// a real breaking change looks like too. This is the evidence that tells those
/// two apart, and it has to come from the browser rather than the document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Cookie name → expiry as a Unix timestamp. `None` is a session cookie.
    pub cookies: BTreeMap<String, Option<i64>>,
    /// Where the tab actually ended up. A redirect to a sign-in page is the
    /// loudest available "you are not signed in".
    pub final_url: String,
}

/// Why a scan must not be admitted to a signed-in series.
///
/// Every one of these means "this scan may have run signed out", and a scan
/// that may have run signed out cannot become that series' contract: a
/// signed-out page is a signed-in page with fewer tools, which is also exactly
/// what a real breaking change looks like. Refusing to record is the only
/// answer that cannot be wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    /// Cookies the baseline had are simply not there any more.
    SessionGone { missing: Vec<String> },
    /// They are there, and past their expiry.
    SessionExpired { expired: Vec<String> },
    /// The tab ended up somewhere else — a sign-in wall, most likely.
    Redirected { from: String, to: String },
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionGone { missing } => write!(
                f,
                "the sign-in looks gone — {} is no longer set",
                missing.join(", ")
            ),
            Self::SessionExpired { expired } => {
                write!(
                    f,
                    "the sign-in has expired — {} ran out",
                    expired.join(", ")
                )
            }
            Self::Redirected { to, .. } => {
                write!(
                    f,
                    "the page sent us to {to} instead, which usually means a sign-in wall"
                )
            }
        }
    }
}

impl Session {
    /// Whether this scan may be recorded as the contract of the series whose
    /// baseline is `against`.
    ///
    /// Deliberately blind to the tool list: the classifier's job is to say what
    /// changed, and this one's is to say whether the two sides are even
    /// comparable. Keeping them apart is what lets the classifier stay
    /// deterministic while the fallible half lives here, where the cost of
    /// being wrong is one extra prompt.
    ///
    /// **Limitation worth knowing:** a site that keeps its session somewhere
    /// other than a cookie leaves nothing to check, and only the redirect test
    /// protects it. Better than nothing, and honest about which it is.
    pub fn admits(&self, against: &Session, now: i64) -> std::result::Result<(), Rejection> {
        // Loudest signal first: landing somewhere else is unambiguous in a way
        // a missing cookie is not.
        if path_of(&self.final_url) != path_of(&against.final_url) {
            return Err(Rejection::Redirected {
                from: against.final_url.clone(),
                to: self.final_url.clone(),
            });
        }

        let missing: Vec<String> = against
            .cookies
            .keys()
            .filter(|name| !self.cookies.contains_key(*name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(Rejection::SessionGone { missing });
        }

        let expired: Vec<String> = against
            .cookies
            .keys()
            .filter(|name| {
                self.cookies
                    .get(*name)
                    .and_then(|expiry| *expiry)
                    .is_some_and(|expiry| expiry <= now)
            })
            .cloned()
            .collect();
        if !expired.is_empty() {
            return Err(Rejection::SessionExpired { expired });
        }

        Ok(())
    }
}

/// The path part of a URL, for comparing where two scans ended up.
///
/// Path only: a query string or fragment moving is ordinary application
/// behaviour, while `/app` becoming `/login` is not.
fn path_of(url: &str) -> String {
    url::Url::parse(url)
        .map(|u| u.path().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| url.to_string())
}

/// What the page turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// At least one tool, and the set held still for the settle window.
    ///
    /// The only variant that may be diffed against a baseline — and, for a
    /// signed-in series, only once `session` has been checked against the one
    /// recorded at sign-in.
    Settled {
        snapshot: Box<Snapshot>,
        session: Session,
    },
    /// The API is there, the set held still, and it is empty.
    ///
    /// Kept apart from [`Outcome::Settled`] so that "this page offers no tools"
    /// is a decision the caller makes on purpose rather than something that
    /// falls out of an empty map.
    SettledEmpty,
    /// No `document.modelContext` on the page at all.
    ///
    /// Either it is not a WebMCP page, or this Chrome is too old, or the
    /// origin trial token is missing. The user agent is reported so the reader
    /// can tell which end is at fault.
    Unsupported { user_agent: String },
    /// The deadline arrived while the tool set was still moving.
    Unsettled {
        /// Tools present at the last read.
        seen: usize,
        /// How long ago the set last changed.
        last_change: Duration,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "no Chrome is listening at {debug_url}. Start one with \
         `--remote-debugging-port=9222`, or scan with a launched browser instead."
    )]
    NotListening { debug_url: String },

    #[error(
        "could not find Chrome. Pass an explicit binary, or install Google Chrome \
         (Edge and Chromium work too — both speak the same DevTools protocol)."
    )]
    NoBrowser,

    #[error("failed to start `{binary}`: {source}")]
    Launch {
        binary: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Chrome started but never published a debugging port")]
    NoDebugPort,

    #[error(
        "{product} is too old to read WebMCP pages — the API arrived in Chrome 149. \
         Update Chrome, or install Edge alongside it."
    )]
    BrowserTooOld { product: String },

    #[error("could not talk to Chrome: {0}")]
    Cdp(String),

    #[error("could not open {url}: {reason}")]
    Navigate { url: String, reason: String },

    #[error(
        "{url} never finished loading within {}s, so nothing was read from it",
        .timeout.as_secs()
    )]
    NeverLoaded { url: String, timeout: Duration },

    #[error("the page's `getTools()` failed: {0}")]
    GetTools(String),

    #[error(
        "the page registered a tool with no `name`, so its contract cannot be \
         keyed. A nameless tool would silently vanish from the next diff."
    )]
    NamelessTool,
}

type Result<T> = std::result::Result<T, Error>;

/// Open the page, wait for its tool set to settle, and report what it is.
pub async fn scan(scan: &Scan) -> Result<Outcome> {
    let browser = chrome::Chrome::open(&scan.browser).await?;
    let mut cdp = cdp::Cdp::connect(&browser.ws_url).await?;

    let result = run(&mut cdp, scan).await;

    // Always, including on the error paths: a browser left running holds the
    // profile lock, and the next scan would find it taken.
    browser.close(&mut cdp).await;

    result
}

async fn run(cdp: &mut cdp::Cdp, scan: &Scan) -> Result<Outcome> {
    // Before anything is read, so "this Chrome cannot do WebMCP" never arrives
    // disguised as "this page has no tools".
    chrome::check_version(cdp).await?;

    let target = cdp
        .call(
            "Target.createTarget",
            serde_json::json!({ "url": "about:blank" }),
            None,
        )
        .await?;
    let target_id = target["targetId"]
        .as_str()
        .ok_or_else(|| Error::Cdp("Target.createTarget returned no targetId".into()))?
        .to_string();

    let result = scan_target(cdp, &target_id, scan).await;

    // Best-effort: the tab is Chrome's problem once we are done with it, and a
    // failure to close it must not mask the scan's own answer.
    let _ = cdp
        .call(
            "Target.closeTarget",
            serde_json::json!({ "targetId": target_id }),
            None,
        )
        .await;

    result
}

async fn scan_target(cdp: &mut cdp::Cdp, target_id: &str, scan: &Scan) -> Result<Outcome> {
    let attached = cdp
        .call(
            "Target.attachToTarget",
            serde_json::json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    let session = attached["sessionId"]
        .as_str()
        .ok_or_else(|| Error::Cdp("Target.attachToTarget returned no sessionId".into()))?
        .to_string();
    let session = Some(session.as_str());

    let user_agent = evaluate(cdp, session, "navigator.userAgent")
        .await?
        .as_str()
        .unwrap_or_default()
        .to_string();

    let navigation = cdp
        .call(
            "Page.navigate",
            serde_json::json!({ "url": scan.url }),
            session,
        )
        .await?;
    // Chrome reports a failed navigation in the response rather than as an
    // error, and then leaves an error page behind that loads perfectly well.
    // Without this check that error page reads as "a site with no WebMCP".
    if let Some(reason) = navigation["errorText"].as_str() {
        return Err(Error::Navigate {
            url: scan.url.clone(),
            reason: reason.to_string(),
        });
    }

    settle(cdp, session, scan, &user_agent).await
}

/// Poll until the tool set stops moving, or the deadline arrives.
async fn settle(
    cdp: &mut cdp::Cdp,
    session: Option<&str>,
    scan: &Scan,
    user_agent: &str,
) -> Result<Outcome> {
    let deadline = Instant::now() + scan.timeout;
    let mut loaded = false;
    let mut last_seen: Option<String> = None;
    let mut last_change = Instant::now();
    let mut count = 0usize;

    loop {
        let probe = evaluate(cdp, session, PROBE_JS).await?;

        if let Some(error) = probe["error"].as_str() {
            return Err(Error::GetTools(error.to_string()));
        }

        if probe["ready"].as_str() == Some("complete") {
            if !probe["supported"].as_bool().unwrap_or(false) {
                return Ok(Outcome::Unsupported {
                    user_agent: user_agent.to_string(),
                });
            }

            if !loaded {
                loaded = true;
                last_change = Instant::now();
            }

            let tools = probe["tools"].as_array().cloned().unwrap_or_default();
            count = tools.len();
            // Compared canonically so that a re-serialisation with keys in a
            // different order does not read as the set having moved.
            let fingerprint = schemadiff::canonical_json(&serde_json::Value::Array(tools.clone()));

            if last_seen.as_deref() != Some(fingerprint.as_str()) {
                last_seen = Some(fingerprint);
                last_change = Instant::now();
            } else if last_change.elapsed() >= scan.settle {
                if tools.is_empty() {
                    return Ok(Outcome::SettledEmpty);
                }
                let snapshot = Box::new(snapshot::build(&scan.url, &tools)?);
                let final_url = probe["href"].as_str().unwrap_or(&scan.url).to_string();
                return Ok(Outcome::Settled {
                    snapshot,
                    session: session_of(cdp, session, final_url).await,
                });
            }
        }

        if Instant::now() >= deadline {
            return if loaded {
                Ok(Outcome::Unsettled {
                    seen: count,
                    last_change: last_change.elapsed(),
                })
            } else {
                Err(Error::NeverLoaded {
                    url: scan.url.clone(),
                    timeout: scan.timeout,
                })
            };
        }

        tokio::time::sleep(POLL).await;
    }
}

/// Read the page's cookie names and expiries out of the browser.
///
/// Best-effort on purpose: a browser that will not answer leaves an empty
/// fingerprint, and an empty fingerprint fails the caller's check rather than
/// passing it. Losing the evidence must never read as "the session is fine".
async fn session_of(cdp: &mut cdp::Cdp, session: Option<&str>, final_url: String) -> Session {
    let response = cdp
        .call("Network.getCookies", serde_json::json!({}), session)
        .await
        .unwrap_or_default();

    let cookies = response["cookies"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|cookie| {
                    let name = cookie["name"].as_str()?.to_string();
                    // Chrome reports -1 for a session cookie.
                    let expires = cookie["expires"].as_f64().filter(|e| *e > 0.0);
                    Some((name, expires.map(|e| e as i64)))
                })
                .collect()
        })
        .unwrap_or_default();

    Session { cookies, final_url }
}

async fn evaluate(
    cdp: &mut cdp::Cdp,
    session: Option<&str>,
    expression: &str,
) -> Result<serde_json::Value> {
    let response = cdp
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "awaitPromise": true,
                "returnByValue": true,
            }),
            session,
        )
        .await?;

    if let Some(details) = response.get("exceptionDetails") {
        let text = details["exception"]["description"]
            .as_str()
            .or_else(|| details["text"].as_str())
            .unwrap_or("the page threw");
        return Err(Error::Cdp(text.to_string()));
    }

    Ok(response["result"]["value"].clone())
}
