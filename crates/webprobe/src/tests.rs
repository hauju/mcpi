//! Mapping tests. Nothing here starts a browser — see `tests/scan.rs` for that.

use serde_json::json;

use crate::snapshot::build;

fn tool(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "description": "A tool.",
        "inputSchema": { "type": "object", "properties": {} },
    })
}

#[test]
fn keys_tools_by_name_and_keeps_the_whole_object() {
    let snapshot = build("https://app.example.com/todos", &[tool("add_todo")]).unwrap();

    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools["add_todo"], tool("add_todo"));
}

#[test]
fn names_the_origin_not_the_page() {
    // Two routes of one app must produce the same `server_name`, or every scan
    // of a different page would read as a renamed server.
    let a = build("https://app.example.com/todos?x=1", &[tool("t")]).unwrap();
    let b = build("https://app.example.com/settings", &[tool("t")]).unwrap();

    assert_eq!(a.server_name, "https://app.example.com");
    assert_eq!(a.server_name, b.server_name);
}

#[test]
fn leaves_versions_empty_so_they_never_diff() {
    let snapshot = build("https://app.example.com", &[tool("t")]).unwrap();

    assert!(snapshot.protocol_version.is_empty());
    assert!(snapshot.server_version.is_empty());
    assert!(snapshot.resources.is_empty());
    assert!(snapshot.prompts.is_empty());
}

#[test]
fn refuses_a_nameless_tool() {
    // Keying it would be guesswork, and dropping it would make the tool
    // reappear as "removed" in the next diff against this snapshot.
    let nameless = json!({ "description": "no name" });

    assert!(matches!(
        build("https://app.example.com", &[nameless]),
        Err(crate::Error::NamelessTool)
    ));
}

#[test]
fn a_page_that_only_rewords_is_cosmetic() {
    let before = build("https://app.example.com", &[tool("t")]).unwrap();
    let after = build(
        "https://app.example.com",
        &[json!({
            "name": "t",
            "description": "A tool, reworded.",
            "inputSchema": { "type": "object", "properties": {} },
        })],
    )
    .unwrap();

    let diff = schemadiff::diff(&before, &after);
    assert_eq!(diff.severity(), Some(schemadiff::Severity::Cosmetic));
}

/// Admission: whether a scan is even comparable to the series' baseline.
mod admission {
    use std::collections::BTreeMap;

    use crate::{Rejection, Session};

    const NOW: i64 = 1_800_000_000;

    fn session(url: &str, cookies: &[(&str, Option<i64>)]) -> Session {
        Session {
            cookies: cookies
                .iter()
                .map(|(name, expiry)| (name.to_string(), *expiry))
                .collect(),
            final_url: url.into(),
        }
    }

    fn signed_in() -> Session {
        session(
            "https://app.example.com/todos",
            &[("sid", None), ("csrf", Some(NOW + 3600))],
        )
    }

    #[test]
    fn the_same_session_is_admitted() {
        assert_eq!(signed_in().admits(&signed_in(), NOW), Ok(()));
    }

    #[test]
    fn a_vanished_cookie_is_refused() {
        // The failure this whole gate exists for: the session quietly lapsed,
        // the page still renders, and it offers three public tools instead of
        // twelve. Nothing about the tool list says which happened.
        let now = session(
            "https://app.example.com/todos",
            &[("csrf", Some(NOW + 3600))],
        );

        assert_eq!(
            now.admits(&signed_in(), NOW),
            Err(Rejection::SessionGone {
                missing: vec!["sid".into()]
            })
        );
    }

    #[test]
    fn an_expired_cookie_is_refused() {
        let now = session(
            "https://app.example.com/todos",
            &[("sid", None), ("csrf", Some(NOW - 1))],
        );

        assert!(matches!(
            now.admits(&signed_in(), NOW),
            Err(Rejection::SessionExpired { .. })
        ));
    }

    #[test]
    fn a_redirect_to_a_sign_in_wall_is_refused() {
        let now = session("https://app.example.com/login", &[("sid", None)]);

        assert!(matches!(
            now.admits(&signed_in(), NOW),
            Err(Rejection::Redirected { .. })
        ));
    }

    #[test]
    fn a_query_string_is_not_a_redirect() {
        // Apps put filters and tracking params in the URL. Treating those as a
        // sign-in wall would make the gate cry wolf until it got switched off.
        let now = session(
            "https://app.example.com/todos?filter=open",
            &[("sid", None), ("csrf", Some(NOW + 3600))],
        );

        assert_eq!(now.admits(&signed_in(), NOW), Ok(()));
    }

    #[test]
    fn an_extra_cookie_is_not_a_problem() {
        // Analytics and consent banners add cookies constantly. Only what the
        // baseline had is evidence of anything.
        let mut extra = signed_in();
        extra.cookies.insert("_ga".into(), Some(NOW + 99));

        assert_eq!(extra.admits(&signed_in(), NOW), Ok(()));
    }

    #[test]
    fn losing_the_evidence_fails_closed() {
        // `session_of` yields an empty fingerprint when the browser will not
        // answer. That must read as "cannot vouch for this scan", never as
        // "the session is fine".
        let blank = Session {
            cookies: BTreeMap::new(),
            final_url: "https://app.example.com/todos".into(),
        };

        assert!(blank.admits(&signed_in(), NOW).is_err());
    }
}
