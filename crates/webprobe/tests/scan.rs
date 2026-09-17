//! End-to-end scans against the fixture pages in a real headless Chrome.
//!
//! Ignored by default: they need a Chrome on the machine, which a contributor
//! may not have and which `cargo test` should not fail over. CI runs them
//! explicitly with `--ignored` (see `just test-chrome`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use webprobe::{Browser, Outcome, Scan, scan};

/// Serve `fixtures/` on a loopback port, and return its base URL.
///
/// A handful of lines rather than a dependency: the pages are static, the
/// client is one browser, and nothing here needs to outlive the test.
async fn serve_fixtures() -> String {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let root = root.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let Ok(read) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_string();

                let response = match std::fs::read(root.join(&path)) {
                    Ok(body) => {
                        let mime = if path.ends_with(".js") {
                            "text/javascript"
                        } else {
                            "text/html"
                        };
                        let mut head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        head.extend_from_slice(&body);
                        head
                    }
                    Err(_) => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };

                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    base
}

fn headless(url: String) -> Scan {
    Scan::new(url, Browser::anonymous())
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn reads_a_settled_tool_set() {
    let base = serve_fixtures().await;
    let outcome = scan(&headless(format!("{base}/v1.html"))).await.unwrap();

    let Outcome::Settled { snapshot, .. } = outcome else {
        panic!("expected a settled contract, got {outcome:?}");
    };
    assert_eq!(snapshot.tools.len(), 2);
    assert!(snapshot.tools.contains_key("add_todo"));
    assert_eq!(snapshot.server_name, base);
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn waits_for_tools_that_register_late() {
    // The regression this whole crate is shaped around: `slow.html` has one
    // tool at load and a second 1.2s later. Reading once would record a
    // contract that was never true.
    let base = serve_fixtures().await;
    let outcome = scan(&headless(format!("{base}/slow.html"))).await.unwrap();

    let Outcome::Settled { snapshot, .. } = outcome else {
        panic!("expected a settled contract, got {outcome:?}");
    };
    assert_eq!(snapshot.tools.len(), 2, "the late tool was missed");
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn tells_no_api_apart_from_no_tools() {
    let base = serve_fixtures().await;

    let plain = scan(&headless(format!("{base}/plain.html"))).await.unwrap();
    assert!(
        matches!(plain, Outcome::Unsupported { .. }),
        "got {plain:?}"
    );

    let empty = scan(&headless(format!("{base}/empty.html"))).await.unwrap();
    assert_eq!(empty, Outcome::SettledEmpty);
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn refuses_to_snapshot_a_page_that_keeps_moving() {
    let base = serve_fixtures().await;
    let mut scan_spec = headless(format!("{base}/churn.html"));
    scan_spec.timeout = Duration::from_secs(6);
    let outcome = scan(&scan_spec).await.unwrap();

    assert!(
        matches!(outcome, Outcome::Unsettled { .. }),
        "a churning page must not produce a snapshot, got {outcome:?}"
    );
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn two_fixture_versions_diff_as_breaking() {
    let base = serve_fixtures().await;

    let before = scan(&headless(format!("{base}/v1.html"))).await.unwrap();
    let after = scan(&headless(format!("{base}/v2.html"))).await.unwrap();

    let (
        Outcome::Settled {
            snapshot: before, ..
        },
        Outcome::Settled {
            snapshot: after, ..
        },
    ) = (before, after)
    else {
        panic!("both fixtures should settle");
    };

    // `add_todo` gained a required argument; `search` only reworded.
    let diff = schemadiff::diff(&before, &after);
    assert_eq!(diff.severity(), Some(schemadiff::Severity::Breaking));
}

/// Start a Chrome of our own and return its DevTools endpoint.
///
/// Stands in for the browser a user already has open: the desktop app attaches
/// rather than launching, so that path needs a Chrome it did not start.
async fn users_chrome() -> (tokio::process::Child, tempfile::TempDir, String) {
    let profile = tempfile::tempdir().unwrap();
    let binary = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ]
    .iter()
    .map(Path::new)
    .find(|p| p.is_file())
    .map(Path::to_path_buf)
    .or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("google-chrome"))
            .find(|p| p.is_file())
    })
    .expect("a local Chrome");

    let child = tokio::process::Command::new(binary)
        .arg("--headless=new")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg("--no-first-run")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn chrome");

    let port_file = profile.path().join("DevToolsActivePort");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(&port_file)
            && let Some(port) = text
                .lines()
                .next()
                .and_then(|l| l.trim().parse::<u16>().ok())
        {
            return (child, profile, format!("http://127.0.0.1:{port}"));
        }
        assert!(
            std::time::Instant::now() < deadline,
            "chrome never listened"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn attaches_to_a_browser_it_did_not_start() {
    // The desktop app's path. Attaching is what keeps the user's session, and
    // so what makes tools behind a sign-in visible at all.
    let base = serve_fixtures().await;
    let (mut chrome, _profile, debug_url) = users_chrome().await;

    let outcome = scan(&Scan::new(
        format!("{base}/v1.html"),
        Browser::Attached { debug_url },
    ))
    .await
    .unwrap();

    let Outcome::Settled { snapshot, .. } = outcome else {
        panic!("expected a settled contract, got {outcome:?}");
    };
    assert_eq!(snapshot.tools.len(), 2);

    let _ = chrome.kill().await;
}

#[tokio::test]
async fn says_so_when_no_browser_is_listening() {
    // The likeliest first run: Chrome is open, but not with a debugging port.
    // Needs no browser itself, so it is not ignored.
    let error = scan(&Scan::new(
        "https://app.example.com",
        Browser::Attached {
            // Port 1 is privileged and never a DevTools endpoint.
            debug_url: "http://127.0.0.1:1".into(),
        },
    ))
    .await
    .unwrap_err();

    assert!(
        error.to_string().contains("--remote-debugging-port"),
        "the error must say how to fix it, got: {error}"
    );
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn a_named_profile_remembers_and_a_throwaway_does_not() {
    // The whole reason the app owns a profile: whatever a sign-in leaves
    // behind has to still be there on the next scan, or every rescan would
    // read the signed-out contract and diff as mass tool removal.
    let base = serve_fixtures().await;
    let url = format!("{base}/remember.html");
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("chrome");

    let named = || Scan::new(url.clone(), Browser::profile(&profile));

    let first = scan(&named()).await.unwrap();
    let Outcome::Settled {
        snapshot: first, ..
    } = first
    else {
        panic!("expected a contract, got {first:?}");
    };
    assert!(first.tools.contains_key("first_visit"));

    let again = scan(&named()).await.unwrap();
    let Outcome::Settled {
        snapshot: again, ..
    } = again
    else {
        panic!("expected a contract, got {again:?}");
    };
    assert!(
        again.tools.contains_key("returning"),
        "the profile forgot between scans: {:?}",
        again.tools.keys().collect::<Vec<_>>()
    );

    // And the CI shape stays signed out every time, by construction.
    let anonymous = scan(&Scan::new(url, Browser::anonymous())).await.unwrap();
    let Outcome::Settled {
        snapshot: anonymous,
        ..
    } = anonymous
    else {
        panic!("expected a contract, got {anonymous:?}");
    };
    assert!(anonymous.tools.contains_key("first_visit"));
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn reads_a_page_through_the_browsers_own_api() {
    // Everything else here runs against a polyfill, which would pass on a
    // Chrome that has never heard of WebMCP. This one does not: the fixture
    // registers nothing unless the browser itself provides the API, so it is
    // the only test that can fail when the launch flags stop working.
    let base = serve_fixtures().await;
    let outcome = scan(&headless(format!("{base}/native.html")))
        .await
        .unwrap();

    let Outcome::Settled { snapshot, .. } = outcome else {
        panic!(
            "the browser did not expose document.modelContext — check the \
             --enable-features flag: {outcome:?}"
        );
    };
    assert!(snapshot.tools.contains_key("native_tool"));
}
