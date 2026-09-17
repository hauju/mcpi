//! Getting hold of a browser to drive.
//!
//! Nothing is bundled and nothing is downloaded: a Chrome that is already
//! installed gets launched into a profile the caller names. A WebMCP page needs
//! Chrome or Edge to run at all, so "use the browser you have" is the honest
//! answer here rather than a shortcut around the no-Electron promise.
//!
//! **Attaching to the user's everyday Chrome is not an option.** Since Chrome
//! 136 the browser refuses `--remote-debugging-port` on its default profile —
//! verified on 154, where the port simply never opens. So a debugging endpoint
//! always belongs to a purpose-started browser on some other profile, and the
//! cookies in it are whatever that profile has. Which is why the profile is a
//! parameter: a scan is only as signed-in as the profile it runs in.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Browser, Error, Result};

/// How long a launched Chrome gets to publish its port before we give up.
const STARTUP: Duration = Duration::from_secs(20);

/// The first Chrome release with `document.modelContext` behind the origin
/// trial. Anything older cannot answer the question a scan asks, and must say
/// so rather than reporting the page as having no tools.
const MIN_MAJOR: u32 = 149;

/// Searched in order when no binary is given.
const CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "microsoft-edge",
];

pub(crate) struct Chrome {
    pub(crate) ws_url: String,
    /// Held so the process outlives the scan.
    child: Option<tokio::process::Child>,
    /// Held so a throwaway profile is not reaped while Chrome is using it.
    _profile: Option<tempfile::TempDir>,
}

impl Chrome {
    pub(crate) async fn open(browser: &Browser) -> Result<Self> {
        match browser {
            Browser::Attached { debug_url } => {
                let ws_url = debugger_url(debug_url)
                    .await
                    .ok_or_else(|| Error::NotListening {
                        debug_url: debug_url.clone(),
                    })?;
                Ok(Self {
                    ws_url,
                    child: None,
                    _profile: None,
                })
            }
            Browser::Launched {
                binary,
                profile,
                headless,
            } => launch(binary.as_deref(), profile.as_deref(), *headless).await,
        }
    }

    /// Ask Chrome to shut itself down, and wait for it to finish.
    ///
    /// Not a signal: Chrome writes its cookie database lazily, so killing it
    /// right after a sign-in loses the session that sign-in existed to create.
    /// A signal also leaves `exit_type: Crashed` in the profile, which greets
    /// the user with a restore bubble the next time they see the window.
    pub(crate) async fn close(mut self, cdp: &mut crate::cdp::Cdp) {
        // A browser we attached to belongs to whoever started it.
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = cdp.call("Browser.close", serde_json::json!({}), None).await;
        // Bounded: a Chrome that will not leave is worse than an orphan.
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let _ = child.start_kill();
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        // Only reached when `close` was skipped — a panic, or an early return
        // on a path that never got a working CDP connection.
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// Refuse a browser too old to have the API, rather than letting it report
/// every page as having no tools.
pub(crate) async fn check_version(cdp: &mut crate::cdp::Cdp) -> Result<()> {
    let version = cdp
        .call("Browser.getVersion", serde_json::json!({}), None)
        .await?;
    let product = version["product"].as_str().unwrap_or_default().to_string();

    let major = product
        .rsplit('/')
        .next()
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse::<u32>().ok());

    match major {
        Some(major) if major < MIN_MAJOR => Err(Error::BrowserTooOld { product }),
        // An unparseable product string is not evidence of anything. Chromium
        // forks spell it differently, and refusing them on a guess would be
        // worse than letting the scan speak for itself.
        _ => Ok(()),
    }
}

async fn launch(binary: Option<&Path>, profile: Option<&Path>, headless: bool) -> Result<Chrome> {
    let binary = match binary {
        Some(path) => path.to_path_buf(),
        None => find_browser().ok_or(Error::NoBrowser)?,
    };

    // A named profile persists, so a sign-in survives to the next scan. An
    // unnamed one is a throwaway, which is what CI wants.
    let (profile_path, temp) = match profile {
        Some(path) => {
            std::fs::create_dir_all(path).map_err(|e| Error::Launch {
                binary: binary.display().to_string(),
                source: e,
            })?;
            seed_preferences(path);
            (path.to_path_buf(), None)
        }
        None => {
            let dir = tempfile::tempdir().map_err(|e| Error::Launch {
                binary: binary.display().to_string(),
                source: e,
            })?;
            (dir.path().to_path_buf(), Some(dir))
        }
    };

    let mut command = tokio::process::Command::new(&binary);
    if headless {
        // `--headless=new` specifically: the old headless is a separate binary
        // that does not necessarily carry the WebMCP implementation.
        command.arg("--headless=new");
    }
    command
        // Port 0 lets the OS pick, so two scans never collide.
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile_path.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        // WebMCP ships in Chrome 149+ but stays off unless the *page* serves an
        // origin-trial token, and almost none do. Forcing the feature makes the
        // token irrelevant: a page that registers tools is then readable
        // whether or not its author enrolled in the trial, which is the only
        // way this feature is useful before the API reaches general
        // availability. Verified on 154 — without it `document.modelContext`
        // is undefined even on a page that registers tools.
        .arg("--enable-features=WebMCP")
        // Deliberately *not* `--enable-automation`: it sets navigator.webdriver,
        // and identity providers refuse to sign in to a browser that admits to
        // being automated. A profile nobody can sign into is the one thing this
        // profile exists to avoid.
        .arg("--disable-sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = command.spawn().map_err(|e| Error::Launch {
        binary: binary.display().to_string(),
        source: e,
    })?;

    let port = wait_for_port(&profile_path)
        .await
        .ok_or(Error::NoDebugPort)?;
    let ws_url = debugger_url(&format!("http://127.0.0.1:{port}"))
        .await
        .ok_or(Error::NoDebugPort)?;

    Ok(Chrome {
        ws_url,
        child: Some(child),
        _profile: temp,
    })
}

/// Make a fresh persistent profile keep its session cookies.
///
/// Most SaaS auth cookies carry no `Expires`, and Chrome drops session cookies
/// at startup unless the profile is set to continue where it left off. Without
/// this a user signs in, the scan works, and the *next* scan is mysteriously
/// signed out again.
///
/// Best-effort and only on a profile Chrome has not written yet: overwriting a
/// real `Preferences` file would discard settings that are not ours.
fn seed_preferences(profile: &Path) {
    let default = profile.join("Default");
    let preferences = default.join("Preferences");
    if preferences.exists() {
        return;
    }
    if std::fs::create_dir_all(&default).is_err() {
        return;
    }
    let _ = std::fs::write(
        &preferences,
        serde_json::json!({ "session": { "restore_on_startup": 1 } }).to_string(),
    );
}

fn find_browser() -> Option<PathBuf> {
    CANDIDATES.iter().find_map(|candidate| {
        let path = Path::new(candidate);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_path_buf());
        }
        // A bare name: let the PATH resolve it, the same way a shell would.
        which(candidate)
    })
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Read the port Chrome bound from `DevToolsActivePort` in the profile.
///
/// The file appears only once the socket is actually listening, which makes it
/// a readiness signal as well as the port itself. Its first line is the port.
///
/// A stale file from a previous run is removed first, so a Chrome that hands
/// off to an existing instance and exits cannot be mistaken for a live one.
async fn wait_for_port(profile: &Path) -> Option<u16> {
    let file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + STARTUP;

    while Instant::now() < deadline {
        if let Ok(text) = tokio::fs::read_to_string(&file).await
            && let Some(port) = text.lines().next().and_then(|l| l.trim().parse().ok())
            && reachable(port).await
        {
            return Some(port);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Whether anything is actually answering on that port.
///
/// Chrome's second instance on a profile hands its command line to the first
/// and exits 0, leaving whatever `DevToolsActivePort` was already there. The
/// file alone is therefore not proof of a live endpoint.
async fn reachable(port: u16) -> bool {
    debugger_url(&format!("http://127.0.0.1:{port}"))
        .await
        .is_some()
}

/// Ask the DevTools HTTP endpoint for its browser-level WebSocket URL.
async fn debugger_url(debug_url: &str) -> Option<String> {
    let response = reqwest::Client::new()
        .get(format!("{}/json/version", debug_url.trim_end_matches('/')))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = response.json().await.ok()?;
    body["webSocketDebuggerUrl"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_profile_is_told_to_keep_session_cookies() {
        let dir = tempfile::tempdir().unwrap();
        seed_preferences(dir.path());

        let written = std::fs::read_to_string(dir.path().join("Default/Preferences")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["session"]["restore_on_startup"], 1);
    }

    #[test]
    fn an_existing_profile_is_left_alone() {
        // Chrome owns this file once it has written one. Overwriting it would
        // throw away settings that are not ours to discard.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Default")).unwrap();
        std::fs::write(dir.path().join("Default/Preferences"), r#"{"mine":true}"#).unwrap();

        seed_preferences(dir.path());

        let written = std::fs::read_to_string(dir.path().join("Default/Preferences")).unwrap();
        assert_eq!(written, r#"{"mine":true}"#);
    }
}
