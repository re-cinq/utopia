//! A managed fetch path (#330).
//!
//! Before, each source built its own `reqwest::Client`, six of them, each setting only a timeout and UA, with no
//! single place that asked "where is this request going to connect". So `http://169.254.169.254/latest/meta-data/`
//! would be fetched as a web page, chunked and indexed; redirects followed ten hops by default, so a public host could point
//! the second hop at the internal network; the response body was read into memory in one `bytes()` call, with no limit.
//!
//! **Who chose the address decides how strict to be.**
//!
//! - [`Reach::Operator`]: the address was typed in by an editor of this KB (url / webdav / jira /
//!   custom sources). The internal network is a **legitimate target**: a self-hosted internal wiki is exactly what this product is for,
//!   and `sync_custom` has long turned off the proxy for loopback addresses.
//! - [`Reach::Content`]: the address comes from fetched content (an article link in a feed). Here
//!   "someone chose it" has degraded to "a feed said so", so only public addresses are allowed.
//!
//! What both get: after resolution, **every address is validated**; `resolve_to_addrs` pins the addresses
//! (resolving again after validation is how the DNS rebinding trick works); redirects are followed hop by hop manually
//! and re-checked; https must not be downgraded to http; the response body has a byte limit; and the whole run has an overall time limit.
//!
//! **A redirect must not widen the reach**: when the operator typed a public host, the second hop may not point into the internal network,
//! because they chose that machine, not that machine's forwarding ability.
//!
//! The shape and key implementation points come from the version #326 (@J-i-K) wrote for RSS full-text fetching; here it is extracted into
//! a single path shared by all sources.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::Url;

/// Who chose the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A host typed in by an editor of the KB: the internal network is also allowed
    Operator,
    /// An address given in fetched content: only public addresses
    Content,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_redirects: usize,
    pub dns_timeout: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Option<Duration>,
    pub overall_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_redirects: 5,
            dns_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            read_timeout: None,
            overall_timeout: Duration::from_secs(30),
        }
    }
}

impl Limits {
    pub fn with_overall(mut self, overall: Duration) -> Self {
        self.overall_timeout = overall;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("only http and https are fetched, got {0:?}")]
    UnsupportedScheme(String),
    #[error("destination {0} is not a public address")]
    BlockedAddress(IpAddr),
    #[error("could not resolve {0}")]
    Dns(String),
    #[error("redirect to {0} is not allowed from here")]
    BlockedRedirect(String),
    #[error("too many redirects")]
    RedirectLimit,
    #[error("response is larger than {0} bytes")]
    TooLarge(usize),
    #[error("HTTP {0}")]
    Status(u16),
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

#[derive(Debug)]
pub struct Fetched {
    pub final_url: Url,
    pub mime: String,
    pub content_encoding: Option<reqwest::header::HeaderValue>,
    pub bytes: Vec<u8>,
}

/// A client that can only reach this host, with the address already validated against `reach`.
///
/// For sources that send several requests of their own (GitHub / Jira / custom endpoints / WebDAV all need
/// auth headers, paging, and PROPFIND) — they keep their own request logic and only swap in a
/// managed client for where they connect.
pub async fn client_for(
    url: &Url,
    reach: Reach,
    limits: Limits,
) -> Result<reqwest::Client, FetchError> {
    build_client(url, reach, limits, true).await
}

/// `follow`: `get` follows hop by hop and re-checks by itself, so it needs a client that does **not** follow;
/// sources that send their own requests (GitHub / Jira / WebDAV) keep the library's default follow behavior.
async fn build_client(
    url: &Url,
    reach: Reach,
    limits: Limits,
    follow: bool,
) -> Result<reqwest::Client, FetchError> {
    if reach == Reach::Content {
        validate_content_url(url.as_str())?;
    }
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl(url.to_string()))?;
    let addrs = tokio::time::timeout(limits.dns_timeout, resolve(host, url, reach))
        .await
        .map_err(|_| FetchError::Dns(host.to_string()))??;
    let private = addrs.iter().any(|a| !is_public_ip(a.ip()));
    let mut builder = reqwest::Client::builder()
        .connect_timeout(limits.connect_timeout)
        .timeout(limits.overall_timeout)
        .user_agent(crate::ingest_sources::UA)
        .resolve_to_addrs(host, &addrs);
    if let Some(timeout) = limits.read_timeout {
        builder = builder.read_timeout(timeout);
    }
    if reach == Reach::Content {
        // Proxies can bypass DNS pinning; keep encoding visible to RSS acceptance.
        builder = builder
            .no_proxy()
            .no_hickory_dns()
            .no_gzip()
            .no_brotli()
            .no_zstd();
    }
    if !follow {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    // Internal and loopback do not go through the system proxy: a proxy would only return 502 for these addresses (`sync_custom` has always done this)
    if private {
        builder = builder.no_proxy();
    }
    Ok(builder.build()?)
}

/// GET a URL, following redirects itself.
pub async fn get(raw_url: &str, reach: Reach, limits: Limits) -> Result<Fetched, FetchError> {
    // DNS and redirect validation also consume the overall budget.
    tokio::time::timeout(limits.overall_timeout, get_inner(raw_url, reach, limits))
        .await
        .map_err(|_| FetchError::Status(408))?
}

async fn get_inner(raw_url: &str, reach: Reach, limits: Limits) -> Result<Fetched, FetchError> {
    if reach == Reach::Content {
        validate_content_url(raw_url)?;
    }
    let mut url = parse(raw_url)?;
    let deadline = Instant::now() + limits.overall_timeout;

    for hop in 0..=limits.max_redirects {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FetchError::Status(408));
        }
        // **Built-in redirects must be turned off**: which hop to follow is decided by the loop above, re-checked hop by hop;
        // handing it to reqwest to follow would mean nobody sees where it went
        let client = build_client(&url, reach, limits.with_overall(remaining), false).await?;
        let mut request = client.get(url.clone());
        if reach == Reach::Content {
            request = request.header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,text/markdown",
            );
        }
        let response = request
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await?;

        if response.status().is_redirection() {
            if hop == limits.max_redirects {
                return Err(FetchError::RedirectLimit);
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| FetchError::BlockedRedirect("(no location)".into()))?;
            if reach == Reach::Content && raw_authority_contains_userinfo(location) {
                return Err(FetchError::BlockedRedirect("userinfo".into()));
            }
            let next = url
                .join(location)
                .map_err(|_| FetchError::BlockedRedirect(location.to_string()))?;
            check_hop(&url, &next, reach).await?;
            url = next;
            continue;
        }
        if !response.status().is_success() {
            return Err(FetchError::Status(response.status().as_u16()));
        }
        return read_bounded(response, url, limits.max_bytes).await;
    }
    Err(FetchError::RedirectLimit)
}

/// Whether a hop is allowed through.
///
/// Besides the rules of `reach` itself, there is one **no-upgrade** rule: if this hop originally landed on the public internet,
/// the next hop may not land on the internal network. The operator chose that host, not that host's forwarding ability.
async fn check_hop(from: &Url, to: &Url, reach: Reach) -> Result<(), FetchError> {
    if from.scheme() == "https" && to.scheme() == "http" {
        return Err(FetchError::BlockedRedirect(to.to_string()));
    }
    if reach == Reach::Content {
        // build_client will resolve and pin the next hop exactly once.
        validate_content_url(to.as_str())?;
        return Ok(());
    }
    let host = to
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl(to.to_string()))?;
    let addrs = resolve(host, to, reach).await?;
    let from_host = from.host_str().unwrap_or_default();
    let from_public = resolve(from_host, from, reach)
        .await
        .map(|a| a.iter().all(|s| is_public_ip(s.ip())))
        .unwrap_or(true);
    if from_public && addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(FetchError::BlockedRedirect(to.to_string()));
    }
    Ok(())
}

pub(crate) fn validate_content_url(raw: &str) -> Result<Url, FetchError> {
    let parsed =
        Url::parse(raw).map_err(|_| FetchError::InvalidUrl("invalid content URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.fragment().is_some()
        || parsed.port().is_some_and(|port| !matches!(port, 80 | 443))
        || parsed.username() != ""
        || parsed.password().is_some()
        || raw_authority_contains_userinfo(raw)
    {
        return Err(FetchError::InvalidUrl("invalid content URL".into()));
    }
    if let Some(ip) = parsed.host().and_then(|host| match host {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }) {
        if !is_public_ip(ip) {
            return Err(FetchError::BlockedAddress(ip));
        }
    }
    Ok(parsed)
}

fn raw_authority_contains_userinfo(raw: &str) -> bool {
    let Some(authority) = raw
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
    else {
        return false;
    };
    authority.contains('@')
}

fn parse(raw: &str) -> Result<Url, FetchError> {
    let url = Url::parse(raw.trim()).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(FetchError::UnsupportedScheme(other.to_string())),
    }
}

/// Resolve the host and validate every address against `reach`.
///
/// **Resolve once, then pin**: if the client resolved again after validation, that instant is
/// where DNS rebinding strikes. The port is left as 0; hyper's connector uses the port in the URL.
async fn resolve(host: &str, url: &Url, reach: Reach) -> Result<Vec<SocketAddr>, FetchError> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| FetchError::InvalidUrl(url.to_string()))?;
    let literal = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    };
    let resolved: Vec<SocketAddr> = if let Some(ip) = literal {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| FetchError::Dns(host.to_string()))?
            .collect()
    };
    validate_resolved(resolved, reach, host)
}

fn validate_resolved(
    resolved: Vec<SocketAddr>,
    reach: Reach,
    host: &str,
) -> Result<Vec<SocketAddr>, FetchError> {
    if resolved.is_empty() {
        return Err(FetchError::Dns(host.to_string()));
    }
    if reach == Reach::Content {
        // **If any one fails, reject the whole thing**: do not pick out the public ones and connect to them. A host resolving to both
        // public and internal addresses is itself the method of pulling a request into the internal network
        if let Some(bad) = resolved.iter().find(|a| !is_public_ip(a.ip())) {
            return Err(FetchError::BlockedAddress(bad.ip()));
        }
    }
    let mut seen = HashSet::new();
    Ok(resolved
        .into_iter()
        .filter(|a| seen.insert(a.ip()))
        .map(|a| SocketAddr::new(a.ip(), 0))
        .collect())
}

async fn read_bounded(
    response: reqwest::Response,
    final_url: Url,
    max_bytes: usize,
) -> Result<Fetched, FetchError> {
    let content_encoding = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .cloned();
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "application/octet-stream".into());
    // If Content-Length says it is over the limit, do not start reading; but it is what the other side says, so do not trust it alone
    if response
        .content_length()
        .is_some_and(|n| n as usize > max_bytes)
    {
        return Err(FetchError::TooLarge(max_bytes));
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > max_bytes {
            return Err(FetchError::TooLarge(max_bytes));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Fetched {
        final_url,
        mime,
        content_encoding,
        bytes,
    })
}

/// Is this address on the public internet.
///
/// Blocked using the RFC 6890 special-purpose address table, **blocking only what is in the table**: treating all of `192.0.0.0/16` as reserved
/// is a common over-block (only `192.0.0.0/24` and `192.0.2.0/24` are reserved), and blocking one extra range
/// means a legitimate site that a user cannot fetch.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_loopback()          // 127.0.0.0/8
                || v4.is_private()      // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()   // 169.254/16
                || v4.is_broadcast()
                || v4.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
                || v4.is_unspecified()  // 0.0.0.0/8
                || a == 0
                || (a == 100 && (64..=127).contains(&b))   // 100.64/10 CGNAT
                || (a == 192 && b == 0 && c == 0)          // 192.0.0/24 IETF protocol assignments
                || (a == 192 && b == 88 && c == 99)        // 6to4 relay anycast
                || (a == 198 && (b == 18 || b == 19))      // 198.18/15 benchmarking
                || a >= 224) // multicast and reserved
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            // Local-use NAT64 and special-purpose IPv6 cannot become RSS targets.
            if seg[..3] == [0x0064, 0xff9b, 1]
                || (seg[0] == 0x2001 && (seg[1] == 0 || seg[1] == 2 || (seg[1] & 0xfff0) == 0x0010))
                || (seg[0] == 0x3fff && (seg[1] & 0xf000) == 0)
            {
                return false;
            }
            // Preserve the shared NAT64 embedded-address exclusion.
            if seg[0] == 0x0064 && seg[1] == 0xff9b {
                let v4 = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    seg[6] as u8,
                    (seg[7] >> 8) as u8,
                    seg[7] as u8,
                );
                return is_public_ip(IpAddr::V4(v4));
            }
            // 6to4 (2002::/16) likewise: the v4 address is in segments 2 and 3
            if seg[0] == 0x2002 {
                let v4 = std::net::Ipv4Addr::new(
                    (seg[1] >> 8) as u8,
                    seg[1] as u8,
                    (seg[2] >> 8) as u8,
                    seg[2] as u8,
                );
                return is_public_ip(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xff00) == 0xff00      // ff00::/8 multicast
                || (seg[0] & 0xfe00) == 0xfc00      // fc00::/7 unique local
                || (seg[0] & 0xffc0) == 0xfe80      // fe80::/10 link-local
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // 2001:db8::/32 documentation use
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn content_urls_reject_empty_userinfo_before_parsing_erases_it() {
        let error = get("https://@127.0.0.1/", Reach::Content, Limits::default())
            .await
            .unwrap_err();
        assert!(matches!(error, FetchError::InvalidUrl(_)), "{error:?}");
    }

    #[test]
    fn mixed_dns_answers_fail_closed_even_when_one_address_is_public() {
        let addresses = vec![
            "93.184.216.34:0".parse().unwrap(),
            "192.168.1.10:0".parse().unwrap(),
        ];
        assert!(matches!(
            validate_resolved(addresses, Reach::Content, "example.com"),
            Err(FetchError::BlockedAddress(_))
        ));
    }

    #[tokio::test]
    async fn content_redirects_preserve_the_rss_boundary() {
        let from = Url::parse("https://8.8.8.8/article").unwrap();
        for target in [
            "http://8.8.8.8/final",
            "https://127.0.0.1/admin",
            "https://[::1]/",
            "https://8.8.8.8:8443/",
            "https://user:pass@8.8.8.8/",
            "https://8.8.8.8/#fragment",
            "ftp://8.8.8.8/",
        ] {
            assert!(
                check_hop(&from, &Url::parse(target).unwrap(), Reach::Content)
                    .await
                    .is_err(),
                "{target}"
            );
        }
        assert!(
            check_hop(&from, &from.join("/final").unwrap(), Reach::Content)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn shared_fetch_obeys_read_and_overall_deadlines() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header("accept-encoding", "identity"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("late")
                    .set_delay(Duration::from_millis(250)),
            )
            .mount(&server)
            .await;
        for limits in [
            Limits {
                read_timeout: Some(Duration::from_millis(20)),
                ..Limits::default()
            },
            Limits {
                overall_timeout: Duration::from_millis(20),
                ..Limits::default()
            },
        ] {
            let result = get(&server.uri(), Reach::Operator, limits).await;
            assert!(
                matches!(result, Err(FetchError::Status(408)))
                    || matches!(result, Err(FetchError::Transport(ref e)) if e.is_timeout()),
                "{result:?}"
            );
        }
    }

    #[test]
    fn content_reach_preserves_rss_special_ipv6_exclusions() {
        for address in [
            "64:ff9b:1::808:808",
            "2001::1",
            "2001:10::1",
            "2001:2::1",
            "3fff::1",
        ] {
            assert!(!is_public_ip(ip(address)), "{address}");
        }
    }

    #[test]
    fn the_addresses_a_fetch_must_refuse() {
        for blocked in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.5",
            "169.254.169.254", // the cloud metadata service; this is the path most often abused
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1", // v4-mapped loopback
            "64:ff9b::7f00:1",  // NAT64 wrapping loopback
            "2002:7f00:1::",    // 6to4 wrapping loopback
        ] {
            assert!(!is_public_ip(ip(blocked)), "{blocked} should be blocked");
        }
    }

    #[test]
    fn the_addresses_a_fetch_must_still_allow() {
        for allowed in [
            "1.1.1.1",
            "8.8.8.8",
            "192.0.1.1",   // the part of 192.0.0.0/16 that is **not** reserved
            "192.88.98.1", // the 6to4 relay only occupies 192.88.99.0/24
            "2606:4700::1111",
            "2002:0808:0808::", // 6to4 wrapping 8.8.8.8
        ] {
            assert!(is_public_ip(ip(allowed)), "{allowed} should not be blocked");
        }
    }

    /// Actually run a local server and hit it: loopback is a **private address**, so these assertions sit exactly on
    /// the boundary between the two reaches: an address given by content cannot reach it, while one the operator typed can.
    #[tokio::test]
    async fn a_loopback_host_is_reachable_only_when_a_person_typed_it() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/page"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw("<html><body>hello</body></html>", "text/html"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/page", server.uri());

        // A link in a feed points at 127.0.0.1 -- exactly what this path is meant to block
        let refused = get(&url, Reach::Content, Limits::default()).await;
        assert!(
            matches!(refused, Err(FetchError::InvalidUrl(_))),
            "内容给出的地址不该够得到回环，得到的是 {refused:?}"
        );

        // The same address, but the source was typed in by this KB's own person: an internal service is a legitimate target
        let page = get(&url, Reach::Operator, Limits::default())
            .await
            .expect("the internal address entered by the operator should be fetchable");
        assert_eq!(page.mime, "text/html");
        assert!(String::from_utf8_lossy(&page.bytes).contains("hello"));
    }

    #[tokio::test]
    async fn a_body_larger_than_the_cap_is_refused() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("x".repeat(4096)))
            .mount(&server)
            .await;
        // Before, this was a single `resp.bytes()`: whatever the other side returned was read in full
        let limits = Limits {
            max_bytes: 1024,
            ..Limits::default()
        };
        let out = get(&server.uri(), Reach::Operator, limits).await;
        assert!(matches!(out, Err(FetchError::TooLarge(1024))), "{out:?}");
    }

    #[tokio::test]
    async fn a_redirect_chain_ends_instead_of_looping() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(302).insert_header("location", "/next"))
            .mount(&server)
            .await;
        let limits = Limits {
            max_redirects: 2,
            ..Limits::default()
        };
        let out = get(&server.uri(), Reach::Operator, limits).await;
        assert!(matches!(out, Err(FetchError::RedirectLimit)), "{out:?}");
    }

    #[test]
    fn only_http_and_https_are_fetched() {
        assert!(parse("file:///etc/passwd").is_err());
        assert!(parse("gopher://example.com/").is_err());
        assert!(parse("https://example.com/a").is_ok());
    }
}
