//! Runs the real binary against the real mockserver fixture.
//!
//! Nothing is mocked: the CLI is spawned as a process, it spawns the fixture
//! over stdio, and the assertions are on stdout and the exit code — the two
//! things a pipeline actually consumes.

use std::path::PathBuf;
use std::process::Command;

use mcpstore::Store;

/// Locate the fixture binary next to this test executable — same trick as
/// mcpclient's stdio tests, and `just test` builds it first for the same
/// reason.
fn mockserver_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("test executable has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let binary = dir.join("mockserver");
    assert!(
        binary.exists(),
        "fixture not built at {}. Run `cargo build -p mockserver` first \
         (`just test` and CI both do this).",
        binary.display()
    );
    binary
}

fn fixture(variant: &str) -> String {
    format!(
        "stdio:{} --variant {variant}",
        mockserver_binary().display()
    )
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mcpi-cli"))
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mcpi-cli-test-{}-{name}", std::process::id()))
}

#[test]
fn an_unchanged_contract_exits_zero() {
    let out = cli()
        .args(["diff", &fixture("a"), &fixture("a")])
        .output()
        .expect("binary runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("No contract changes"));
}

#[test]
fn a_breaking_change_exits_one_and_says_why() {
    let out = cli()
        .args(["diff", &fixture("a"), &fixture("b")])
        .output()
        .expect("binary runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("breaking"), "{stdout}");
}

#[test]
fn a_snapshot_file_round_trips_through_diff() {
    let path = scratch("snapshot.json");
    let out = cli()
        .args(["snapshot", &fixture("a"), "--out"])
        .arg(&path)
        .output()
        .expect("binary runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // File vs the live server it came from: nothing changed, exit 0.
    let same = cli()
        .args(["diff", &path.display().to_string(), &fixture("a")])
        .output()
        .expect("binary runs");
    assert_eq!(same.status.code(), Some(0));

    // File vs the other variant: the recorded contract broke, exit 1.
    let broke = cli()
        .args(["diff", &path.display().to_string(), &fixture("b")])
        .output()
        .expect("binary runs");
    assert_eq!(broke.status.code(), Some(1));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_baseline_pinned_in_the_apps_store_is_a_valid_source() {
    // Build a store the way the app would: record variant a, pin it.
    let db = scratch("store.db");
    let _ = std::fs::remove_file(&db);
    {
        let store = Store::open(&db).expect("store opens");
        let id = store
            .add_server(mcpstore::NewServer {
                name: "fixture".into(),
                transport_kind: mcpstore::TransportKind::Stdio,
                config: serde_json::json!({}),
            })
            .unwrap();

        // Get variant a's snapshot through the CLI itself, so the file format
        // and the store agree by construction.
        let json = cli()
            .args(["snapshot", &fixture("a")])
            .output()
            .expect("binary runs");
        let snapshot: schemadiff::Snapshot =
            serde_json::from_slice(&json.stdout).expect("snapshot parses");
        let outcome = store.record_snapshot(id, &snapshot).unwrap();
        store.set_snapshot_label(outcome.id(), Some("v1")).unwrap();
    }

    let out = cli()
        .args(["diff", "@v1", &fixture("b"), "--store"])
        .arg(&db)
        .output()
        .expect("binary runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The report names the baseline it compared against.
    assert!(String::from_utf8_lossy(&out.stdout).contains("@v1"));

    let missing = cli()
        .args(["diff", "@nope", &fixture("b"), "--store"])
        .arg(&db)
        .output()
        .expect("binary runs");
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("@v1"));

    let _ = std::fs::remove_file(&db);
}

/// The fixture follows every MUST, so notes alone must not fail a pipeline —
/// a lint that exits 1 on advice would get removed from CI within a week.
#[test]
fn lint_notes_alone_exit_zero() {
    let out = cli()
        .args(["lint", &fixture("a")])
        .output()
        .expect("binary runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0 warnings"), "report was: {stdout}");
    assert!(stdout.contains("2026-07-28"));
}

/// A violated MUST exits 1, which is the entire point of running it in CI.
#[test]
fn lint_exits_one_on_a_must_violation() {
    let path = scratch("lint-broken.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "protocol_version": "2026-07-28",
            "server_name": "broken",
            "server_version": "0.0.0",
            "tools": { "no_schema": { "description": "d" } }
        })
        .to_string(),
    )
    .unwrap();

    let out = cli().arg("lint").arg(&path).output().expect("binary runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("input-schema-invalid"));

    let _ = std::fs::remove_file(&path);
}
