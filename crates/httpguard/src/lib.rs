//! Connect only to public addresses, including DNS answers, redirects, and
//! literal IPs in URLs obtained from remote metadata. No environment proxies:
//! a proxy would resolve the destination outside this policy.
use reqwest::{
    Method, Response,
    dns::{Addrs, Name, Resolve, Resolving},
};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && c == 0)
                || v.is_documentation()
                || (a == 198 && (b == 18 || b == 19)))
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v.segments();
            // Global unicast only; exclude special-purpose, documentation and
            // 6to4 ranges (which can encode a private IPv4 destination).
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

pub fn validate_url(raw: &str) -> Result<url::Url, Error> {
    let url = url::Url::parse(raw)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(
            io::Error::other("Only HTTP(S) URLs without embedded credentials are allowed").into(),
        );
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) if is_public(ip.into()) => {}
        Some(url::Host::Ipv6(ip)) if is_public(ip.into()) => {}
        Some(url::Host::Domain(_)) => {}
        _ => return Err(io::Error::other("The destination is not a public address").into()),
    }
    Ok(url)
}

#[derive(Debug)]
struct PublicDns;
impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((name.as_str(), 0)),
            )
            .await??
            .collect();
            if addresses.is_empty() || addresses.iter().any(|a| !is_public(a.ip())) {
                return Err(io::Error::other(
                    "DNS did not resolve exclusively to public addresses",
                )
                .into());
            }
            // Return exactly the vetted addresses to the connector. There is
            // no second lookup between validation and the TCP connection.
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// The caller must validate the initial URL before passing this raw client to
/// a transport. Prefer `Client` for ordinary requests; it checks every send.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(PublicDns))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if redirect_allowed(attempt.previous(), attempt.url())
                && validate_url(attempt.url().as_str()).is_ok()
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
}

/// Whether a redirect to `next` may be followed after the hops in `previous`
/// (the first being the URL the caller named).
///
/// Five hops at most, http(s) only, on the named host, and never back down to
/// `http` once a hop was `https`. reqwest strips `Authorization` on a host
/// change but keeps every other header, so a same-host `307` to plain `http`
/// would send a pasted `X-API-Key` and the request body in the clear.
pub fn redirect_allowed(previous: &[url::Url], next: &url::Url) -> bool {
    let Some(named) = previous.first() else {
        return false;
    };
    let downgrade = next.scheme() == "http" && previous.iter().any(|u| u.scheme() == "https");
    previous.len() < 5
        && matches!(next.scheme(), "http" | "https")
        && named.host_str().is_some_and(|h| next.host_str() == Some(h))
        && !downgrade
}

#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    public_only: bool,
}
impl Client {
    pub fn public(timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            inner: client_builder().timeout(timeout).build()?,
            public_only: true,
        })
    }
    /// Preserve local development support for a desktop/CLI caller.
    pub fn unrestricted(inner: reqwest::Client) -> Self {
        Self {
            inner,
            public_only: false,
        }
    }
    pub fn public_no_redirects(timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            inner: client_builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            public_only: true,
        })
    }
    pub fn get(&self, url: &str) -> Request {
        self.request(Method::GET, url)
    }
    pub fn post(&self, url: &str) -> Request {
        self.request(Method::POST, url)
    }
    pub fn request(&self, method: Method, url: &str) -> Request {
        Request {
            inner: self.inner.request(method, url),
            public_only: self.public_only,
        }
    }
}
pub struct Request {
    inner: reqwest::RequestBuilder,
    public_only: bool,
}
impl Request {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.inner = self.inner.header(name, value);
        self
    }
    pub fn json<T: serde::Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.inner = self.inner.json(body);
        self
    }
    pub async fn send(self) -> Result<Response, Error> {
        let (client, request) = self.inner.build_split();
        let request = request?;
        if self.public_only {
            validate_url(request.url().as_str())?;
        }
        Ok(client.execute(request).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_private_literals_and_special_ranges() {
        for ip in [
            "0.1.2.3",
            "10.0.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.1.1",
            "192.168.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2002:a00:1::",
            "2001:db8::1",
            "fc00::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()));
        }
        for url in [
            "http://2130706433",
            "http://[::ffff:127.0.0.1]",
            "http://a:b@example.com",
            "file:///etc/hosts",
        ] {
            assert!(validate_url(url).is_err());
        }
    }
    #[test]
    fn redirects_stay_on_the_host_and_never_drop_tls() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        let allowed = |prev: &[&str], next: &str| {
            let prev: Vec<_> = prev.iter().map(|p| u(p)).collect();
            redirect_allowed(&prev, &u(next))
        };
        assert!(allowed(&["https://a.com/mcp"], "https://a.com/mcp/"));
        assert!(allowed(&["http://a.com/mcp"], "https://a.com/mcp"));
        assert!(!allowed(&["https://a.com/mcp"], "http://a.com/mcp"));
        assert!(!allowed(
            &["http://a.com", "https://a.com"],
            "http://a.com/x"
        ));
        assert!(!allowed(&["https://a.com/mcp"], "https://b.com/mcp"));
        assert!(!allowed(&["https://a.com/mcp"], "ftp://a.com/mcp"));
        assert!(!allowed(&["https://a.com/1"; 5], "https://a.com/6"));
    }
    #[tokio::test]
    async fn refuses_localhost_at_connection_time_and_literal_metadata() {
        let client = Client::public(Duration::from_secs(2)).unwrap();
        assert!(client.get("http://localhost:1").send().await.is_err());
        assert!(
            client
                .get("http://127.0.0.1:1/metadata")
                .send()
                .await
                .is_err()
        );
    }
}
