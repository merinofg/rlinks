//! Fetching user-supplied URLs (page metadata, preview images) with SSRF protection, plus the
//! favicon provider. Port of linkding's `http_client`, `website_loader`, `favicon_loader` and
//! `preview_image_loader`.
//!
//! SSRF guard (same policy as linkding): only publicly routable addresses unless allowed by
//! `LD_ALLOWED_INTERNAL_HOSTS`. Hostnames are checked in a custom DNS resolver that returns only
//! the checked addresses, so the connection goes to exactly what was validated (no DNS rebinding
//! gap) and every redirect hop is covered. IP-literal URLs skip DNS and are checked up front and
//! in the redirect policy. Proxies from the environment are disabled for these requests because
//! they would bypass the guard.

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, redirect};
use scraper::{Html, Selector};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::{Host, Url};

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PAGE_BYTES: usize = 5000 * 1024;
const MAX_PREVIEW_BYTES: u64 = 5 * 1024 * 1024;
const MAX_FAVICON_BYTES: usize = 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/101.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub title: Option<String>,
    pub description: Option<String>,
    pub preview_image: Option<String>,
}

pub struct Fetcher {
    guarded: Client,
    allow: Arc<Allowlist>,
    favicon_provider: String,
    /// No network access at all (tests that don't need it).
    offline: bool,
    /// The add form fetches metadata, then the background job needs it again for the preview image.
    cache: Mutex<VecDeque<(String, Metadata)>>,
}

impl Fetcher {
    pub fn new(allowed_internal_hosts: &str, favicon_provider: &str) -> Result<Fetcher> {
        let allow = Arc::new(Allowlist::parse(allowed_internal_hosts));
        let policy_allow = allow.clone();
        let guarded = Client::builder()
            .no_proxy()
            .dns_resolver(Arc::new(GuardedResolver(allow.clone())))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    attempt.error("too many redirects")
                } else if let Err(e) = policy_allow.check_url(attempt.url()) {
                    attempt.error(e)
                } else {
                    attempt.follow()
                }
            }))
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Fetcher {
            guarded,
            allow,
            favicon_provider: favicon_provider.to_string(),
            offline: false,
            cache: Mutex::default(),
        })
    }

    /// A fetcher that never touches the network: metadata is empty, downloads fail.
    pub fn offline() -> Fetcher {
        Fetcher {
            offline: true,
            ..Fetcher::new("", "").expect("default client builds")
        }
    }

    /// A GET request through the SSRF guard. Fails up front when offline, for schemes other than
    /// http(s), and for IP-literal hosts that aren't allowed.
    fn request(&self, url: &str) -> Result<reqwest::RequestBuilder> {
        if self.offline {
            bail!("network access disabled");
        }
        let url = Url::parse(url)?;
        self.allow.check_url(&url).map_err(|e| anyhow!(e))?;
        Ok(self.guarded.get(url))
    }

    /// Title, description and preview image of a page. Failures yield empty metadata (linkding).
    pub async fn metadata(&self, url: &str) -> Metadata {
        if let Some((_, m)) = self.cache.lock().unwrap().iter().find(|(u, _)| u == url) {
            return m.clone();
        }
        let meta = match self.load_page(url).await {
            Ok(html) => parse_metadata(url, &html),
            Err(e) => {
                tracing::debug!("load {url}: {e:#}");
                Metadata::default()
            }
        };
        let mut cache = self.cache.lock().unwrap();
        cache.push_front((url.to_string(), meta.clone()));
        cache.truncate(10);
        meta
    }

    /// The page up to `</head>` (all metadata lives there), at most 5 MB.
    async fn load_page(&self, url: &str) -> Result<String> {
        let mut res = self
            .request(url)?
            .header("Accept", "text/html,application/xhtml+xml,application/xml")
            .header("Dnt", "1")
            .send()
            .await?;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            buf.extend_from_slice(&chunk);
            if let Some(end) = buf.windows(7).position(|w| w.eq_ignore_ascii_case(b"</head>")) {
                buf.truncate(end + 7);
                break;
            }
            if buf.len() > MAX_PAGE_BYTES {
                break;
            }
        }
        // ponytail: assumes UTF-8 (lossy); add charset detection if non-UTF-8 sites matter.
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// GET a user-supplied URL through the SSRF guard, at most `max` bytes. Returns the body and
    /// its media type (lowercase, without parameters).
    pub async fn fetch(&self, url: &str, max: usize) -> Result<(Bytes, String)> {
        let res = self.request(url)?.send().await?.error_for_status()?;
        if res.content_length().is_some_and(|len| len > max as u64) {
            bail!("response larger than {max} bytes");
        }
        let media_type = media_type(&res);
        Ok((read_limited(res, max).await?, media_type))
    }

    /// Download a preview image: 2xx, declared size within limits, jpg/png/webp only.
    /// Returns the bytes and file extension.
    pub async fn preview_image(&self, image_url: &str) -> Result<(Bytes, &'static str)> {
        let res = self.request(image_url)?.send().await?.error_for_status()?;
        let len = res.content_length().ok_or_else(|| anyhow!("no Content-Length"))?;
        if len > MAX_PREVIEW_BYTES {
            bail!("preview image too large: {len} bytes");
        }
        let ext = match media_type(&res).as_str() {
            "image/jpeg" => ".jpg",
            "image/png" => ".png",
            "image/webp" => ".webp",
            other => bail!("unsupported preview image type {other:?}"),
        };
        Ok((read_limited(res, len as usize).await?, ext))
    }

    /// Favicon for the page's site from the configured provider. Goes through the SSRF guard
    /// too: provider templates such as `https://{domain}/favicon.ico` reach the bookmarked host.
    pub async fn favicon(&self, page_url: &str) -> Result<Bytes> {
        let (site, domain) = site_of(page_url).ok_or_else(|| anyhow!("no host in {page_url}"))?;
        let provider_url = self
            .favicon_provider
            .replace("{url}", &site)
            .replace("{domain}", &domain);
        let res = self.request(&provider_url)?.send().await?.error_for_status()?;
        read_limited(res, MAX_FAVICON_BYTES).await
    }
}

/// The response body, failing once it grows past `max` bytes.
async fn read_limited(mut res: reqwest::Response, max: usize) -> Result<Bytes> {
    let mut buf = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        buf.extend_from_slice(&chunk);
        if buf.len() > max {
            bail!("response larger than {max} bytes");
        }
    }
    Ok(buf.into())
}

/// The response's media type, lowercase and without parameters (`text/html`).
fn media_type(res: &reqwest::Response) -> String {
    res.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// `("https://example.com", "example.com")` for a page URL: favicons are per site.
pub fn site_of(url: &str) -> Option<(String, String)> {
    let u = Url::parse(url).ok()?;
    let host = u.host_str()?.to_string();
    Some((format!("{}://{host}", u.scheme()), host))
}

pub fn parse_metadata(url: &str, html: &str) -> Metadata {
    let doc = Html::parse_document(html);
    let first_content = |selector: &str| {
        let sel = Selector::parse(selector).expect("valid selector");
        doc.select(&sel)
            .next()
            .and_then(|e| e.value().attr("content"))
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
    };
    let title_sel = Selector::parse("title").expect("valid selector");
    let title = doc
        .select(&title_sel)
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty());
    let description =
        first_content(r#"meta[name="description"]"#).or_else(|| first_content(r#"meta[property="og:description"]"#));
    let preview_image = first_content(r#"meta[property="og:image"]"#).and_then(|img| {
        if img.starts_with("http://") || img.starts_with("https://") {
            Some(img)
        } else {
            Url::parse(url).ok()?.join(&img).ok().map(String::from)
        }
    });
    Metadata {
        title,
        description,
        preview_image,
    }
}

// ---------------------------------------------------------------------------------------------
// SSRF guard

struct GuardedResolver(Arc<Allowlist>);

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.0.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if !allow.host_allowed(&host)
                && let Some(bad) = addrs.iter().find(|a| !allow.addr_allowed(a.ip()))
            {
                return Err(blocked(&host, bad.ip()).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn blocked(host: &str, ip: IpAddr) -> String {
    format!(
        "refusing to connect to {host}, which resolves to the non-public address {ip}; \
         use LD_ALLOWED_INTERNAL_HOSTS to allow it"
    )
}

/// `LD_ALLOWED_INTERNAL_HOSTS`: comma-separated `*`, hostnames (`.example.com` also matches
/// subdomains), IP addresses and CIDR ranges.
#[derive(Debug, Default)]
pub struct Allowlist {
    all: bool,
    hosts: Vec<String>,
    nets: Vec<(IpAddr, u8)>,
}

impl Allowlist {
    pub fn parse(value: &str) -> Allowlist {
        let mut a = Allowlist::default();
        for entry in value
            .split(',')
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
        {
            if entry == "*" {
                a.all = true;
                continue;
            }
            let entry = entry.trim_start_matches('[').trim_end_matches(']');
            let (addr, len) = entry.split_once('/').unwrap_or((entry, ""));
            match addr.parse::<IpAddr>() {
                Ok(ip) => {
                    let max = if ip.is_ipv4() { 32 } else { 128 };
                    match if len.is_empty() { Ok(max) } else { len.parse::<u8>() } {
                        Ok(l) if l <= max => a.nets.push((ip, l)),
                        _ => tracing::warn!("ignoring invalid network in LD_ALLOWED_INTERNAL_HOSTS: {entry}"),
                    }
                }
                Err(_) if entry.contains('/') => {
                    tracing::warn!("ignoring invalid network in LD_ALLOWED_INTERNAL_HOSTS: {entry}")
                }
                Err(_) => a.hosts.push(entry.to_string()),
            }
        }
        a
    }

    fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let host = host.trim_end_matches('.');
        self.all
            || self.hosts.iter().any(|h| match h.strip_prefix('.') {
                Some(domain) => host == domain || host.ends_with(h.as_str()),
                None => host == h,
            })
    }

    fn addr_allowed(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        self.all || is_public(ip) || self.nets.iter().any(|&(net, len)| in_network(ip, net, len))
    }

    /// Only http(s); IP-literal hosts must be allowed (hostnames are checked at DNS resolution).
    fn check_url(&self, url: &Url) -> Result<(), String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("unsupported URL scheme {}", url.scheme()));
        }
        let ip = match url.host() {
            Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
            Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
            Some(Host::Domain(_)) => return Ok(()),
            None => return Err("URL has no host".into()),
        };
        if self.host_allowed(&ip.to_string()) || self.addr_allowed(ip) {
            Ok(())
        } else {
            Err(blocked(&ip.to_string(), ip))
        }
    }
}

fn in_network(ip: IpAddr, net: IpAddr, len: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => prefix_eq(u32::from(a).into(), u32::from(n).into(), len, 32),
        (IpAddr::V6(a), IpAddr::V6(n)) => prefix_eq(u128::from(a), u128::from(n), len, 128),
        _ => false,
    }
}

fn prefix_eq(a: u128, b: u128, len: u8, bits: u8) -> bool {
    len == 0 || (a ^ b) >> (bits - len) == 0
}

/// Publicly routable per the IANA special-purpose registries (what Python's `is_global` says,
/// which linkding uses), plus linkding's extra IPv6 exclusions.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let x = u128::from(v6);
            let v6net = |s: &str, len| prefix_eq(x, u128::from(s.parse::<std::net::Ipv6Addr>().unwrap()), len, 128);
            // Global unicast is 2000::/3; everything else (loopback, ULA fc00::/7, link-local,
            // multicast, NAT64 64:ff9b::/96, IPv4-compatible ::/96, site-local...) is not public.
            v6net("2000::", 3) && !v6net("2001::", 23) && !v6net("2001:db8::", 32) && !v6net("2002::", 16)
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    const NON_PUBLIC: &[([u8; 4], u8)] = &[
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([100, 64, 0, 0], 10),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 2, 0], 24),
        ([192, 88, 99, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([224, 0, 0, 0], 4),
        ([240, 0, 0, 0], 4),
    ];
    !NON_PUBLIC
        .iter()
        .any(|&(net, len)| prefix_eq(u32::from(ip).into(), u32::from(Ipv4Addr::from(net)).into(), len, 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.20.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::808:808",
            "::127.0.0.1",
            "fec0::1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn allowlist() {
        let a = Allowlist::parse("nas.local, .corp.example, 192.168.1.0/24, [fd00::1], bad/range, 10.0.0.0/99");
        assert!(a.host_allowed("nas.local") && a.host_allowed("NAS.local."));
        assert!(a.host_allowed("corp.example") && a.host_allowed("wiki.corp.example"));
        assert!(!a.host_allowed("evilcorp.example") && !a.host_allowed("other.local"));
        assert!(a.addr_allowed("192.168.1.77".parse().unwrap()));
        assert!(
            a.addr_allowed("::ffff:192.168.1.77".parse().unwrap()),
            "v4-mapped matches v4 ranges"
        );
        assert!(a.addr_allowed("fd00::1".parse().unwrap()));
        assert!(!a.addr_allowed("192.168.2.1".parse().unwrap()));
        assert!(
            !a.addr_allowed("10.0.0.1".parse().unwrap()),
            "invalid ranges are ignored"
        );
        assert!(Allowlist::parse("*").addr_allowed("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn url_check() {
        let a = Allowlist::default();
        let check = |u: &str| a.check_url(&Url::parse(u).unwrap());
        assert!(
            check("https://example.com").is_ok(),
            "hostnames are checked at resolution"
        );
        assert!(check("http://8.8.8.8/x").is_ok());
        for bad in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/latest",
            "http://0x7f.1/",
            "file:///etc/passwd",
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn localhost_name_is_blocked_at_resolution() {
        let f = Fetcher::new("", "").unwrap();
        let err = f.load_page("http://localhost:1/").await.unwrap_err();
        assert!(format!("{err:#}").contains("non-public address"), "{err:#}");
        // allowlisted: passes the guard (then fails to connect, which is fine)
        let f = Fetcher::new("localhost", "").unwrap();
        let err = f.load_page("http://localhost:1/").await.unwrap_err();
        assert!(!format!("{err:#}").contains("non-public address"), "{err:#}");
    }

    #[test]
    fn metadata_parsing() {
        let html = r#"<html><head><title> Hello </title>
            <meta property="og:description" content="og desc">
            <meta property="og:image" content="/img/p.png"></head>"#;
        let m = parse_metadata("https://ex.com/a/b", html);
        assert_eq!(m.title.as_deref(), Some("Hello"));
        assert_eq!(m.description.as_deref(), Some("og desc"));
        assert_eq!(m.preview_image.as_deref(), Some("https://ex.com/img/p.png"));
        let m = parse_metadata(
            "https://ex.com",
            r#"<meta name="description" content=" d "><meta property="og:description" content="x">"#,
        );
        assert_eq!((m.title, m.description.as_deref()), (None, Some("d")));
    }

    #[test]
    fn sites() {
        let site = |u| site_of(u).map(|s| s.0);
        assert_eq!(
            site("https://www.example.com/path?x=1").as_deref(),
            Some("https://www.example.com")
        );
        assert_eq!(site("http://localhost:8080/").as_deref(), Some("http://localhost"));
        assert_eq!(site("mailto:x@y.z"), None);
    }

    #[tokio::test]
    async fn favicon_provider_is_guarded() {
        let f = Fetcher::new("", "http://{domain}:1/favicon.ico").unwrap();
        let err = f.favicon("http://127.0.0.1/page").await.unwrap_err();
        assert!(format!("{err:#}").contains("non-public address"), "{err:#}");
        let f = Fetcher::new("", "http://localhost:1/icon?url={url}").unwrap();
        let err = f.favicon("https://example.com/").await.unwrap_err();
        assert!(format!("{err:#}").contains("non-public address"), "{err:#}");
    }
}
