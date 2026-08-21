//! Marking the tools a server will not let you call without credentials.
//!
//! MCP has no field for "this tool needs auth", so the app has two signals and
//! they are not equally trustworthy. A tool whose last call came back an auth
//! error is a fact we watched happen. A description that mentions an account is
//! the server's own prose, which may just as easily be describing what the tool
//! returns. Both earn a lock, but never the same one: a guess and a fact have
//! to stay legible as different things, or the mark stops meaning anything.
//!
//! Neither reading belongs in `mcpstore` — the store keeps the call log, and
//! deciding what an error *was about* is a heuristic over English.

use chrono::{DateTime, Utc};

/// Why a tool is shown as locked, and how well we know it.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthHint {
    /// The tool's most recent call came back as an auth error, at this time.
    Observed(DateTime<Utc>),
    /// Only the tool's own description claims credentials are needed.
    Declared,
}

/// Whether a failed tool result reads as a refusal for want of credentials.
///
/// Deliberately broad: this only runs on calls that already failed, so the
/// cost of a wide net is a lock on a tool that is broken for some other
/// reason — which still tells you the last call did not work.
pub fn error_is_auth(response: &str) -> bool {
    const MARKERS: &[&str] = &[
        "authentic", // authenticate / authenticated / authentication
        "unauthorized",
        "forbidden",
        "401",
        "403",
        "access token",
        "api key",
        "apikey",
        "sign in",
        "log in",
        "login",
        "credential",
        "connect your",
    ];
    let text = response.to_lowercase();
    MARKERS.iter().any(|m| text.contains(m))
}

/// Whether a tool's description states that it needs credentials.
///
/// Delegates to `probe`, which runs the same test from outside a session to
/// decide whether an endpoint lists gated tools without a challenge. One
/// definition on purpose: if these drifted, the app would lock a tool the
/// endpoint report calls open, and neither surface would be wrong on its own.
pub fn declares_auth(description: &str) -> bool {
    probe::declares_auth(description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trustmrr_description_declares_auth() {
        assert!(declares_auth(
            "Browse active TrustMRR startups with bounded filters, sorting, and \
             pagination. Requires a connected TrustMRR account in ChatGPT or a \
             TrustMRR API key in other MCP clients."
        ));
    }

    #[test]
    fn describing_auth_without_demanding_it_is_not_a_hint() {
        assert!(!declares_auth(
            "Returns the authenticated user's projects, newest first."
        ));
        assert!(!declares_auth(
            "Show the intentionally limited public snapshot."
        ));
    }

    #[test]
    fn trustmrr_error_reads_as_auth() {
        assert!(error_is_auth(
            r#"{"content":[{"type":"text","text":"Connect your TrustMRR account to use this tool."}]}"#
        ));
    }

    #[test]
    fn an_ordinary_failure_is_not_an_auth_failure() {
        assert!(!error_is_auth(
            r#"{"content":[{"type":"text","text":"No startup with slug 'nope'."}]}"#
        ));
    }
}
