//! Best-effort removal of credential-shaped strings from a `find_tools` request before it is sent
//! to Jev, traced, or written to the decision log.
//!
//! Only recognisable shapes are caught: known key prefixes, JWTs, the token after `Bearer`/`Basic`,
//! and the value of a secret-named `key=value`. A bare password or an unprefixed hex key passes
//! through, so this narrows what leaks rather than guaranteeing nothing does. Ids and paths are left
//! alone on purpose — they are what decides which tool applies.

const REDACTED: &str = "[redacted]";

/// Prefixes of provider keys. A token counts only with at least [`MIN_TAIL`] characters after the
/// prefix, so words like `sk-learn` survive.
const PREFIXES: &[&str] = &[
    "oat_",
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "rk_test_",
    "whsec_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "AKIA",
    "AIza",
    "ya29.",
    "shpat_",
    "npm_",
    "hf_",
];
const MIN_TAIL: usize = 16;

/// A `key=value` whose key ends with one of these has its value redacted.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_key",
    "private_key",
];

pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_scheme = false;
    let mut rest = text;
    while let Some(start) = rest.find(is_token_char) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c| !is_token_char(c)).unwrap_or(rest.len());
        let token = &rest[..end];
        rest = &rest[end..];

        if (after_scheme && token.len() >= 8) || is_secret(token) {
            out.push_str(REDACTED);
        } else if let Some((key, value)) = token.split_once('=')
            && !value.is_empty()
            && is_secret_key(key)
        {
            out.push_str(key);
            out.push('=');
            out.push_str(REDACTED);
        } else {
            out.push_str(token);
        }
        after_scheme = token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("basic");
    }
    out.push_str(rest);
    out
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-.+/=~".contains(c)
}

fn is_secret(token: &str) -> bool {
    let jwt = token.starts_with("eyJ") && token.matches('.').count() == 2 && token.len() >= 20;
    jwt || PREFIXES
        .iter()
        .any(|p| token.starts_with(p) && token.len() >= p.len() + MIN_TAIL)
}

fn is_secret_key(key: &str) -> bool {
    let key = key.trim_start_matches('-').to_ascii_lowercase();
    SECRET_KEYS.iter().any(|k| key.ends_with(k))
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn redacts_prefixed_keys_and_jwts() {
        assert_eq!(
            scrub("list repos with ghp_abcdefghijklmnop1234 please"),
            "list repos with [redacted] please"
        );
        assert_eq!(
            scrub("token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig_abc"),
            "token [redacted]"
        );
        assert_eq!(scrub("use oat_0123456789abcdef0."), "use [redacted]");
    }

    #[test]
    fn redacts_after_auth_scheme_and_secret_named_values() {
        assert_eq!(
            scrub("Authorization: Bearer abc123def456"),
            "Authorization: Bearer [redacted]"
        );
        assert_eq!(
            scrub("connect with --password=hunter22 and API_KEY=xyz"),
            "connect with --password=[redacted] and API_KEY=[redacted]"
        );
    }

    #[test]
    fn keeps_ids_paths_and_lookalike_words() {
        let text = "open issue 4f9c2a1e-8b7d-4c3e-9a10-2b5d6e7f8a90 in /srv/app/config.toml, \
                    then install sk-learn; page=2";
        assert_eq!(scrub(text), text);
    }
}
