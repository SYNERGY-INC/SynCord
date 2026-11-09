//! Link previews ("embeds"): when a message contains links, the server fetches
//! each page and turns its OpenGraph tags into a small card (site name, title,
//! description, colour, picture).
//!
//! The server fetches instead of the apps so friends' addresses aren't shown to
//! every site that gets linked, and so the same link is only fetched once.
//!
//! Because the server usually runs on someone's home PC, the fetcher must never
//! be usable to reach the home network ("SSRF"). Every address is checked:
//!
//! * only `http`/`https` URLs, without user names or passwords;
//! * host names are resolved by the fetcher itself, and the request is refused
//!   if *any* address is not a public internet address (see [`is_public_ip`]).
//!   The check happens inside the HTTP client's DNS step, so the addresses that
//!   were checked are exactly the ones it connects to (no DNS rebinding);
//! * URLs with a literal IP address follow the same rules;
//! * redirects are followed by hand (at most [`Limits::max_redirects`]) and
//!   every hop is checked again;
//! * system proxy settings are ignored (a proxy would do its own DNS lookup);
//! * time and size limits ([`Limits`]); only `text/html` pages are parsed and
//!   `image/*` URLs become picture embeds; no compression is requested.
//!
//! Only [`Fetcher::new_for_tests`] turns the address check off.

use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use tokio::sync::Semaphore;

use crate::media;

mod rich;
pub use rich::{Providers, RichMeta};

/// User agent sent with every request. Some sites (YouTube, X/Twitter) only
/// include OpenGraph tags for agents that look like a link-preview bot.
pub const USER_AGENT: &str =
    concat!("Mozilla/5.0 (compatible; CelerityBot/", env!("CARGO_PKG_VERSION"), "; +https://github.com/bxpo/SynCord)");

/// Most links previewed per message.
pub const MAX_URLS: usize = 3;
/// Longest title kept, in characters.
pub const MAX_TITLE: usize = 256;
/// Longest description kept, in characters.
pub const MAX_DESCRIPTION: usize = 350;
/// Longest site name kept, in characters.
pub const MAX_SITE_NAME: usize = 100;

const CACHE_ENTRIES: usize = 200;
const CACHE_TTL: Duration = Duration::from_secs(3600);
/// Failures are remembered for a shorter time, so a site that was down briefly gets another chance.
const CACHE_ERROR_TTL: Duration = Duration::from_secs(120);
/// Most picture bytes kept in the cache in total.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Pictures decoded at the same time (decoding is the expensive, memory-hungry part).
const DECODE_SLOTS: usize = 2;

const ACCEPT_PAGE: &str = "text/html,application/xhtml+xml;q=0.9,image/*;q=0.8,*/*;q=0.5";
const ACCEPT_IMAGE: &str = "image/webp,image/png,image/jpeg,image/gif,image/*;q=0.8";

/// Time and size limits for one preview.
#[derive(Debug, Clone)]
pub struct Limits {
    /// For the linked URL: every redirect hop plus reading the page (or the picture, for a direct image link).
    pub page_timeout: Duration,
    /// For downloading the page's picture (`og:image`), separately from `page_timeout`.
    pub image_timeout: Duration,
    /// Most HTML read; reading also stops at `</head>` once the head has OpenGraph tags.
    pub max_html_bytes: usize,
    /// Largest picture downloaded.
    pub max_image_bytes: usize,
    /// Most redirects followed.
    pub max_redirects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            page_timeout: Duration::from_secs(6),
            image_timeout: Duration::from_secs(6),
            max_html_bytes: 1024 * 1024,
            max_image_bytes: 8 * 1024 * 1024,
            max_redirects: 5,
        }
    }
}

/// A finished link preview.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// Final URL after redirects.
    pub url: String,
    pub site_name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Accent colour from `theme-color`, as `0xRRGGBB`.
    pub color: Option<u32>,
    pub image: Option<PreviewImage>,
    /// `og:type`, e.g. `website`, `article`, `video.other`.
    pub kind: Option<String>,
    /// The page says it has a video (`og:video`), e.g. YouTube.
    pub has_video: bool,
    /// The URL itself was a picture or GIF: show it big, with no text card.
    pub is_media: bool,
    /// Extra details for YouTube, X/Twitter and Spotify links.
    pub rich: Option<RichMeta>,
}

/// A picture that was downloaded and checked to be a readable image.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewImage {
    /// The original file, as downloaded (at most [`Limits::max_image_bytes`]).
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
    /// Smaller preview made by [`media::process`] (same sizes as uploads: at most
    /// 480×360 for pictures, 320 px for GIFs). `None` when `bytes` is already small
    /// enough to show as-is (small pictures, small animated GIFs).
    pub thumb: Option<Vec<u8>>,
}

/// Fetches link previews. Create one at start-up and share it; it keeps a small cache.
pub struct Fetcher {
    http: reqwest::Client,
    allow_private: bool,
    /// Only set by this module's tests: one literal address that is let through
    /// even in strict mode, so a strict fetcher can reach a local test server.
    test_exempt_ip: Option<IpAddr>,
    limits: Limits,
    cache: Mutex<Cache>,
    decode: Semaphore,
    /// Where YouTube, X/Twitter and Spotify are asked for details.
    providers: Providers,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher {
    /// A strict fetcher: only public internet addresses.
    pub fn new() -> Self {
        Self::build(false)
    }

    /// **Tests only**: a fetcher that may also reach loopback and private
    /// addresses, so tests can use a local server on 127.0.0.1.
    pub fn new_for_tests() -> Self {
        Self::build(true)
    }

    /// Replace the time and size limits.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Ask other addresses for YouTube, X/Twitter and Spotify details (tests use a local server).
    pub fn with_providers(mut self, providers: Providers) -> Self {
        self.providers = providers;
        self
    }

    fn build(allow_private: bool) -> Self {
        // reqwest is built without a bundled crypto provider; the binary installs
        // ring at start-up, but make sure one is there either way.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(GuardedResolver { allow_private })
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .build()
            .expect("couldn't set up the link preview HTTP client");
        Self {
            http,
            allow_private,
            test_exempt_ip: None,
            limits: Limits::default(),
            cache: Mutex::new(Cache::default()),
            decode: Semaphore::new(DECODE_SLOTS),
            providers: Providers::default(),
        }
    }

    /// Fetch a preview for `url`.
    ///
    /// `Ok(None)` means the link worked but there's nothing worth showing (no
    /// title, description or picture; or a file type that isn't previewed).
    /// `Err` is a short, human-readable reason (refused address, timeout,
    /// error status, ...). Both are cached, so posting the same link again is cheap.
    pub async fn fetch(&self, url: &str) -> Result<Option<Arc<Preview>>, String> {
        let mut url = Url::parse(url.trim()).map_err(|_| "not a valid link".to_string())?;
        url.set_fragment(None);
        let key = url.as_str().to_string();
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return hit;
        }
        let result = self.fetch_uncached(url).await.map(|p| p.map(Arc::new));
        self.cache.lock().unwrap().put(key, &result);
        result
    }

    async fn fetch_uncached(&self, url: Url) -> Result<Option<Preview>, String> {
        if let Some(link) = rich::detect(&url) {
            match self.fetch_rich(&link, &url).await {
                Ok(Some(p)) => return Ok(Some(p)),
                Ok(None) => {}
                // Fall back to the page itself.
                Err(e) => tracing::debug!("no rich preview for {url}: {e}"),
            }
        }
        let page = tokio::time::timeout(self.limits.page_timeout, self.load_page(url))
            .await
            .map_err(|_| "the site took too long to answer".to_string())??;
        match page {
            Page::Other => Ok(None),
            Page::Image { url, bytes } => {
                let Some(image) = self.check_image(bytes).await else { return Ok(None) };
                Ok(Some(Preview {
                    url: url.into(),
                    site_name: None,
                    title: None,
                    description: None,
                    color: None,
                    image: Some(image),
                    kind: None,
                    has_video: false,
                    is_media: true,
                    rich: None,
                }))
            }
            Page::Html { url, meta } => {
                let image = match &meta.image {
                    Some(img_url) => {
                        match tokio::time::timeout(self.limits.image_timeout, self.load_image(img_url.clone())).await {
                            Ok(Ok(bytes)) => self.check_image(bytes).await,
                            Ok(Err(e)) => {
                                tracing::debug!("link preview picture {img_url} skipped: {e}");
                                None
                            }
                            Err(_) => {
                                tracing::debug!("link preview picture {img_url} took too long");
                                None
                            }
                        }
                    }
                    None => None,
                };
                if meta.title.is_none() && meta.description.is_none() && image.is_none() {
                    return Ok(None);
                }
                Ok(Some(Preview {
                    url: url.into(),
                    site_name: meta.site_name,
                    title: meta.title,
                    description: meta.description,
                    color: meta.color,
                    image,
                    kind: meta.kind,
                    has_video: meta.has_video,
                    is_media: false,
                    rich: None,
                }))
            }
        }
    }

    /// Fetch the linked URL and read it as a page or a picture.
    async fn load_page(&self, url: Url) -> Result<Page, String> {
        let (url, mut resp) = self.get(url, ACCEPT_PAGE).await?;
        let (mime, charset) = content_type(resp.headers());
        if mime == "text/html" || mime == "application/xhtml+xml" {
            let bytes = read_html(&mut resp, self.limits.max_html_bytes).await?;
            let meta = parse_meta(&decode_text(&bytes, charset.as_deref()), &url, charset.as_deref(), &bytes);
            Ok(Page::Html { url, meta: Box::new(meta) })
        } else if mime.starts_with("image/") {
            let bytes = read_capped(&mut resp, self.limits.max_image_bytes).await?;
            Ok(Page::Image { url, bytes })
        } else {
            Ok(Page::Other)
        }
    }

    /// Download a page's picture.
    async fn load_image(&self, url: Url) -> Result<Vec<u8>, String> {
        let (_, mut resp) = self.get(url, ACCEPT_IMAGE).await?;
        let (mime, _) = content_type(resp.headers());
        // Some servers label pictures as generic files; the decoder has the final say.
        if !(mime.starts_with("image/") || mime == "application/octet-stream" || mime.is_empty()) {
            return Err(format!("not a picture ({mime})"));
        }
        read_capped(&mut resp, self.limits.max_image_bytes).await
    }

    /// Make sure the bytes are a readable picture and make a preview of it.
    async fn check_image(&self, bytes: Vec<u8>) -> Option<PreviewImage> {
        let _slot = self.decode.acquire().await.ok()?;
        let (bytes, p) = tokio::task::spawn_blocking(move || {
            let p = media::process(&bytes, "image");
            (bytes, p)
        })
        .await
        .ok()?;
        let (width, height) = (p.width?, p.height?);
        Some(PreviewImage { bytes, mime: p.mime, width, height, animated: p.animated, thumb: p.thumb })
    }

    /// GET a URL, following redirects by hand and checking every hop.
    /// Returns the final URL and a successful response.
    async fn get(&self, mut url: Url, accept: &str) -> Result<(Url, reqwest::Response), String> {
        for hop in 0..=self.limits.max_redirects {
            self.check_url(&url)?;
            let resp = self.http.get(url.clone()).header(ACCEPT, accept).send().await.map_err(describe)?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let Some(location) = resp.headers().get(LOCATION) else {
                    return Err(format!("the site answered {status} without saying where to go"));
                };
                if hop == self.limits.max_redirects {
                    return Err("too many redirects".into());
                }
                let location = location.to_str().map_err(|_| "a redirect to an unreadable address".to_string())?;
                url = url.join(location.trim()).map_err(|_| "a redirect to an invalid address".to_string())?;
                url.set_fragment(None);
                continue;
            }
            if !resp.status().is_success() {
                return Err(format!("the site answered {status}"));
            }
            return Ok((url, resp));
        }
        Err("too many redirects".into())
    }

    /// Scheme, credentials and literal-IP checks. Host names are checked when
    /// they are resolved (see [`GuardedResolver`]).
    fn check_url(&self, url: &Url) -> Result<(), String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("only http and https links are previewed".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("links with a user name or password aren't previewed".into());
        }
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or("the link has no host")?;
        // The URL parser has already turned every IPv4 spelling (2130706433, 0x7f.1, ...) into
        // dotted form, so anything that doesn't parse as an address here is a host name.
        let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() else {
            return Ok(());
        };
        if self.allow_private || is_public_ip(ip) || self.test_exempt_ip == Some(ip) {
            Ok(())
        } else {
            Err(Blocked(ip.to_string()).to_string())
        }
    }
}

enum Page {
    Html { url: Url, meta: Box<PageMeta> },
    Image { url: Url, bytes: Vec<u8> },
    Other,
}

/// Content type (lower case, without parameters) and charset of a response.
fn content_type(headers: &reqwest::header::HeaderMap) -> (String, Option<String>) {
    let Some(value) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return (String::new(), None);
    };
    let mut parts = value.split(';');
    let mime = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let charset = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
    });
    (mime, charset)
}

/// Read HTML up to `cap` bytes. Stops early at `</head>` when the head already
/// has OpenGraph tags (some sites, YouTube among them, have been known to put
/// their tags further down, so without them we keep reading up to the cap).
async fn read_html(resp: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(describe)? {
        let from = buf.len().saturating_sub(8);
        buf.extend_from_slice(&chunk);
        if buf.len() >= cap {
            buf.truncate(cap);
            break;
        }
        if let Some(end) = find_ci(&buf, from, b"</head")
            && find_ci(&buf[..end], 0, b"og:title").is_some()
        {
            break;
        }
    }
    Ok(buf)
}

/// Read a whole body, refusing it if it's bigger than `cap`.
async fn read_capped(resp: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let too_big = || "the file is too big to preview".to_string();
    let declared = resp.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|n| n > cap as u64) {
        return Err(too_big());
    }
    let mut buf = Vec::with_capacity(declared.unwrap_or(0).min(cap as u64) as usize);
    while let Some(chunk) = resp.chunk().await.map_err(describe)? {
        if buf.len() + chunk.len() > cap {
            return Err(too_big());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Turn an HTTP client error into a short reason.
fn describe(e: reqwest::Error) -> String {
    let mut source: Option<&(dyn StdError + 'static)> = Some(&e);
    while let Some(s) = source {
        if let Some(blocked) = s.downcast_ref::<Blocked>() {
            return blocked.to_string();
        }
        if let Some(lookup) = s.downcast_ref::<LookupFailed>() {
            return lookup.to_string();
        }
        source = s.source();
    }
    if e.is_timeout() {
        "the site took too long to answer".into()
    } else if e.is_connect() {
        "couldn't connect to the site".into()
    } else {
        "couldn't load the page".into()
    }
}

// ---------------------------------------------------------------------------
// Address checks
// ---------------------------------------------------------------------------

/// The link points at an address that isn't on the public internet.
#[derive(Debug)]
struct Blocked(String);

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "links to private or local addresses ({}) aren't previewed", self.0)
    }
}

impl StdError for Blocked {}

#[derive(Debug)]
struct LookupFailed(String);

impl fmt::Display for LookupFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "couldn't find the site {}", self.0)
    }
}

impl StdError for LookupFailed {}

/// DNS step of the HTTP client: resolves the name and refuses it unless every
/// address is public. The client connects only to the addresses returned here,
/// so what was checked is what gets used.
struct GuardedResolver {
    allow_private: bool,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), 0)).await {
                Ok(addrs) => addrs.collect(),
                Err(_) => Vec::new(),
            };
            if addrs.is_empty() {
                return Err(Box::new(LookupFailed(host)) as Box<dyn StdError + Send + Sync>);
            }
            if !allow_private && let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
                return Err(Box::new(Blocked(format!("{host} is {}", bad.ip()))) as Box<dyn StdError + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether an address is on the public internet (and so fine to fetch from).
///
/// Refused: "this network" 0.0.0.0/8, private 10/8, 172.16/12, 192.168/16,
/// shared/CGNAT 100.64/10 (Tailscale), loopback 127/8, link-local 169.254/16,
/// 192.0.0/24, documentation (192.0.2/24, 198.51.100/24, 203.0.113/24),
/// benchmarking 198.18/15, 6to4 relay 192.88.99/24, multicast, reserved
/// 240/4 and broadcast. For IPv6 only global unicast (2000::/3) is allowed,
/// minus 2001::/23 (incl. Teredo), documentation (2001:db8::/32, 3fff::/20);
/// IPv4-mapped (::ffff:a.b.c.d), NAT64 (64:ff9b::/96) and 6to4 (2002::/16)
/// addresses are judged by the IPv4 address inside.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = a == 0
        || a == 10
        || a == 127
        || a >= 224 // multicast, reserved, broadcast
        || (a == 100 && (b & 0xc0) == 64)
        || (a == 169 && b == 254)
        || (a == 172 && (b & 0xf0) == 16)
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b & 0xfe) == 18)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113);
    !blocked
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // ::ffff:a.b.c.d (IPv4-mapped)
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return is_public_v4(v4(s[6], s[7]));
    }
    // 64:ff9b::a.b.c.d (NAT64)
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return is_public_v4(v4(s[6], s[7]));
    }
    // 2002:aabb:ccdd::/48 (6to4)
    if s[0] == 0x2002 {
        return is_public_v4(v4(s[1], s[2]));
    }
    // Everything outside global unicast: ::, ::1, ::/96, fc00::/7, fe80::/10, fec0::/10, ff00::/8, 100::/64, ...
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    let blocked = (s[0] == 0x2001 && s[1] < 0x0200) // 2001::/23 IETF protocol assignments, Teredo
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x3fff && s[1] < 0x1000); // documentation 3fff::/20
    !blocked
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

type Cached = Result<Option<Arc<Preview>>, String>;

#[derive(Default)]
struct Cache {
    map: HashMap<String, CacheEntry>,
    bytes: usize,
}

struct CacheEntry {
    expires: Instant,
    added: Instant,
    bytes: usize,
    value: Cached,
}

impl Cache {
    fn get(&mut self, key: &str) -> Option<Cached> {
        let entry = self.map.get(key)?;
        if entry.expires > Instant::now() {
            return Some(entry.value.clone());
        }
        self.remove(key);
        None
    }

    fn put(&mut self, key: String, value: &Cached) {
        let now = Instant::now();
        let ttl = if value.is_ok() { CACHE_TTL } else { CACHE_ERROR_TTL };
        let bytes = match value {
            Ok(Some(p)) => p.image.as_ref().map_or(0, |i| i.bytes.len() + i.thumb.as_ref().map_or(0, Vec::len)),
            _ => 0,
        };
        if bytes > CACHE_MAX_BYTES / 4 {
            return;
        }
        self.remove(&key);
        let expired: Vec<String> = self.map.iter().filter(|(_, e)| e.expires <= now).map(|(k, _)| k.clone()).collect();
        for k in expired {
            self.remove(&k);
        }
        while self.map.len() >= CACHE_ENTRIES || self.bytes + bytes > CACHE_MAX_BYTES {
            let Some(oldest) = self.map.iter().min_by_key(|(_, e)| e.added).map(|(k, _)| k.clone()) else { break };
            self.remove(&oldest);
        }
        self.bytes += bytes;
        self.map.insert(key, CacheEntry { expires: now + ttl, added: now, bytes, value: value.clone() });
    }

    fn remove(&mut self, key: &str) {
        if let Some(e) = self.map.remove(key) {
            self.bytes -= e.bytes;
        }
    }
}

// ---------------------------------------------------------------------------
// Finding links in messages
// ---------------------------------------------------------------------------

/// Links in a chat message worth previewing: `http`/`https` only, at most
/// [`MAX_URLS`], without duplicates (ignoring `#fragments`), in the order they
/// appear. Skips links wrapped in `<...>` (Discord's "no preview" syntax) and
/// links inside `` `code` `` or ```` ```code blocks``` ````. Trailing
/// punctuation such as `.,!?:;'"*_~|` is dropped, and a closing `)`/`]`/`}` is
/// only kept when it balances an opening one in the link (Wikipedia links).
/// Returned strings are the links as written (after trimming).
pub fn find_urls(text: &str) -> Vec<String> {
    let masked = mask_code(text);
    let bytes = masked.as_bytes();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 0;
    while out.len() < MAX_URLS {
        let Some(off) = find_ci(bytes, i, b"http") else { break };
        let start = off;
        let rest = &bytes[start + 4..];
        let scheme_len = if rest.starts_with(b"://") {
            7
        } else if rest.len() >= 4 && rest[0].eq_ignore_ascii_case(&b's') && rest[1..].starts_with(b"://") {
            8
        } else {
            i = start + 4;
            continue;
        };
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            i = start + scheme_len;
            continue;
        }
        let mut end = start;
        for (idx, ch) in masked[start..].char_indices() {
            if ch.is_whitespace() || ch.is_control() || matches!(ch, '<' | '>' | '"' | '\u{200b}') {
                break;
            }
            end = start + idx + ch.len_utf8();
        }
        i = end.max(start + scheme_len);
        let suppressed = start > 0 && bytes[start - 1] == b'<' && bytes.get(end) == Some(&b'>');
        if suppressed {
            continue;
        }
        let candidate = trim_trailing(&text[start..end]);
        let Ok(mut parsed) = Url::parse(candidate) else { continue };
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
            continue;
        }
        parsed.set_fragment(None);
        if seen.insert(parsed.as_str().to_string()) {
            out.push(candidate.to_string());
        }
    }
    out
}

/// Replace the contents of `inline code` and ```code blocks``` with spaces
/// (byte for byte, so positions stay the same).
fn mask_code(text: &str) -> String {
    let mut b = text.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let fence: &[u8] = if b[i..].starts_with(b"```") { b"```" } else { b"`" };
        let body = i + fence.len();
        match b[body..].windows(fence.len()).position(|w| w == fence) {
            Some(p) => {
                let end = body + p + fence.len();
                b[i..end].fill(b' ');
                i = end;
            }
            None => i = body,
        }
    }
    // Only whole characters between ASCII backticks were replaced, so this is still UTF-8.
    String::from_utf8(b).unwrap_or_else(|_| text.to_string())
}

fn trim_trailing(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().last() else { return s };
        let drop = match last {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '"' | '*' | '_' | '~' | '|' => true,
            ')' => s.matches('(').count() < s.matches(')').count(),
            ']' => s.matches('[').count() < s.matches(']').count(),
            '}' => s.matches('{').count() < s.matches('}').count(),
            _ => false,
        };
        if !drop {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

// ---------------------------------------------------------------------------
// HTML metadata
// ---------------------------------------------------------------------------

/// What a page says about itself.
#[derive(Debug, Default, Clone, PartialEq)]
struct PageMeta {
    site_name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    /// Absolute http(s) URL of the page's picture.
    image: Option<Url>,
    image_width: Option<u32>,
    image_height: Option<u32>,
    color: Option<u32>,
    kind: Option<String>,
    has_video: bool,
    /// Charset declared in the page (`<meta charset>` or `http-equiv`).
    charset: Option<String>,
    /// Length of a video or song (`itemprop="duration"`, `music:duration`, YouTube's `lengthSeconds`).
    duration_secs: Option<u32>,
    /// `music:musician_description` (Spotify's artist).
    musician: Option<String>,
}

/// Decode a page's bytes: UTF-8 normally, Latin-1/Windows-1252 when declared so.
fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    if charset.is_some_and(is_latin1) {
        bytes.iter().map(|&b| cp1252(b)).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn is_latin1(charset: &str) -> bool {
    matches!(charset, "iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252" | "us-ascii" | "ascii")
}

/// Windows-1252 (which browsers use for "ISO-8859-1") to a character.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    if (0x80..0xa0).contains(&b) { HIGH[(b - 0x80) as usize] } else { b as char }
}

/// Parse `<head>` metadata. `header_charset` is the charset from the HTTP
/// header; if the header didn't give one and the page declares Latin-1, the
/// raw bytes are decoded again that way.
fn parse_meta(html: &str, base: &Url, header_charset: Option<&str>, raw: &[u8]) -> PageMeta {
    let meta = scan_meta(html, base);
    if header_charset.is_none() && meta.charset.as_deref().is_some_and(is_latin1) && std::str::from_utf8(raw).is_err() {
        return scan_meta(&decode_text(raw, meta.charset.as_deref()), base);
    }
    meta
}

/// Walk the tags of a page, collecting `<meta>`, `<title>` and `<base>`.
fn scan_meta(html: &str, page_url: &Url) -> PageMeta {
    let b = html.as_bytes();
    let mut tags: HashMap<String, String> = HashMap::new();
    let mut title_tag: Option<String> = None;
    let mut charset = None;
    let mut base = page_url.clone();
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&c| c == b'<') {
        let start = i + off;
        if b[start..].starts_with(b"<!--") {
            match find(b, start + 4, b"-->") {
                Some(e) => {
                    i = e + 3;
                    continue;
                }
                None => break,
            }
        }
        let closing = b.get(start + 1) == Some(&b'/');
        let name_start = start + 1 + closing as usize;
        let name_len = b[name_start..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'-').count();
        if name_len == 0 {
            i = start + 1;
            continue;
        }
        let name = html[name_start..name_start + name_len].to_ascii_lowercase();
        let (attrs, tag_end) = parse_attrs(html, name_start + name_len);
        i = tag_end;
        if closing {
            continue;
        }
        let attr = |n: &str| attrs.iter().find(|(k, _)| k == n).map(|(_, v)| decode_entities(v));
        match name.as_str() {
            "meta" => {
                if let Some(c) = attr("charset") {
                    charset.get_or_insert(c.trim().to_ascii_lowercase());
                }
                let content = attr("content");
                if attr("http-equiv").is_some_and(|h| h.trim().eq_ignore_ascii_case("content-type"))
                    && let Some(c) = &content
                    && let Some((_, cs)) = c.to_ascii_lowercase().split_once("charset=")
                {
                    charset.get_or_insert(cs.trim().trim_matches(['"', '\'', ';']).to_string());
                }
                let key = attr("property").or_else(|| attr("name")).or_else(|| attr("itemprop"));
                if let (Some(key), Some(content)) = (key, content) {
                    let content = content.trim();
                    if !content.is_empty() {
                        tags.entry(key.trim().to_ascii_lowercase()).or_insert_with(|| content.to_string());
                    }
                }
            }
            "base" => {
                if let Some(href) = attr("href")
                    && let Ok(u) = page_url.join(href.trim())
                {
                    base = u;
                }
            }
            "title" | "script" | "style" | "template" | "textarea" => {
                let close = format!("</{name}");
                let end = find_ci(b, tag_end, close.as_bytes()).unwrap_or(b.len());
                if name == "title" && title_tag.is_none() {
                    title_tag = Some(decode_entities(&html[tag_end..end]));
                }
                i = end;
            }
            _ => {}
        }
    }

    let get = |keys: &[&str]| keys.iter().find_map(|k| tags.get(*k)).cloned();
    let image = get(&["og:image", "og:image:url", "og:image:secure_url", "twitter:image", "twitter:image:src"])
        .and_then(|u| base.join(&u).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    PageMeta {
        site_name: get(&["og:site_name"]).and_then(|s| clean(&s, MAX_SITE_NAME)),
        title: get(&["og:title", "twitter:title"]).or(title_tag).and_then(|s| clean(&s, MAX_TITLE)),
        description: get(&["og:description", "twitter:description", "description"])
            .and_then(|s| clean(&s, MAX_DESCRIPTION)),
        image,
        image_width: get(&["og:image:width"]).and_then(|s| s.trim().parse().ok()),
        image_height: get(&["og:image:height"]).and_then(|s| s.trim().parse().ok()),
        color: get(&["theme-color"]).and_then(|s| parse_color(&s)),
        kind: get(&["og:type"]).and_then(|s| clean(&s, 64)),
        has_video: get(&["og:video", "og:video:url", "og:video:secure_url"]).is_some(),
        charset,
        duration_secs: get(&["duration"])
            .and_then(|d| rich::parse_iso_duration(&d))
            .or_else(|| get(&["music:duration", "video:duration"]).and_then(|d| d.trim().parse().ok()))
            .or_else(|| {
                // YouTube puts the length in its page data rather than a tag.
                let at = html.find("\"lengthSeconds\":\"")? + "\"lengthSeconds\":\"".len();
                html[at..].split('"').next()?.parse().ok()
            }),
        musician: get(&["music:musician_description"]).and_then(|s| clean(&s, MAX_SITE_NAME)),
    }
}

/// Parse a tag's attributes starting just after its name. Returns the
/// attributes (names lower-cased, values raw) and the position after `>`.
fn parse_attrs(html: &str, mut i: usize) -> (Vec<(String, String)>, usize) {
    let b = html.as_bytes();
    let mut attrs = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            return (attrs, b.len());
        }
        if b[i] == b'>' {
            return (attrs, i + 1);
        }
        let name_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        // A stray character like `"` that can't start a name: skip it.
        let name = html[name_start..i].to_ascii_lowercase();
        if i == name_start {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let value = if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let quote = b[j];
                let vs = j + 1;
                let ve = b[vs..].iter().position(|&c| c == quote).map_or(b.len(), |p| vs + p);
                i = (ve + 1).min(b.len());
                &html[vs..ve]
            } else {
                let vs = j;
                while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                i = j;
                &html[vs..j]
            };
            attrs.push((name, value.to_string()));
        } else {
            attrs.push((name, String::new()));
        }
    }
}

/// Collapse whitespace and cut to `max` characters (adding "…"). `None` if empty.
fn clean(s: &str, max: usize) -> Option<String> {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= max {
        return Some(collapsed);
    }
    let mut cut: String = collapsed.chars().take(max - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

/// `#rgb`, `#rrggbb` (alpha in `#rgba`/`#rrggbbaa` ignored) or `rgb(r, g, b)` / `rgba(...)`.
fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        return match hex.len() {
            3 | 4 => {
                let d: Vec<u32> = hex.chars().take(3).map(|c| c.to_digit(16).unwrap() * 17).collect();
                Some(d[0] << 16 | d[1] << 8 | d[2])
            }
            6 | 8 => u32::from_str_radix(&hex[..6], 16).ok(),
            _ => None,
        };
    }
    let lower = s.to_ascii_lowercase();
    let inner = lower.strip_prefix("rgba(").or_else(|| lower.strip_prefix("rgb("))?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    let mut rgb = 0u32;
    for p in &parts[..3] {
        let v: f32 = p.parse().ok()?;
        if !(0.0..=255.0).contains(&v) {
            return None;
        }
        rgb = rgb << 8 | v.round() as u32;
    }
    Some(rgb)
}

/// Decode HTML character references (`&amp;`, `&#39;`, `&#x1F600;`, common named ones).
/// Unknown ones are left as they are.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&semi| semi > 0 && semi <= 32)
            .and_then(|semi| entity(&rest[1..1 + semi]).map(|c| (c, semi + 2)));
        match decoded {
            Some((c, len)) => {
                if let Some(c) = c {
                    out.push(c);
                }
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The character for an entity name (without `&` and `;`). `Some(None)` means
/// "decodes to nothing" (soft hyphen).
fn entity(name: &str) -> Option<Option<char>> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        let c = match code {
            0 => '\u{fffd}',
            0x80..=0x9f => cp1252(code as u8),
            _ => char::from_u32(code).unwrap_or('\u{fffd}'),
        };
        return Some(Some(c));
    }
    let c = match name {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "shy" => return Some(None),
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "laquo" => '«',
        "raquo" => '»',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "frac12" => '½',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "zwj" => '\u{200d}',
        "zwnj" => '\u{200c}',
        "eacute" => 'é',
        "egrave" => 'è',
        "ecirc" => 'ê',
        "aacute" => 'á',
        "agrave" => 'à',
        "acirc" => 'â',
        "iacute" => 'í',
        "oacute" => 'ó',
        "uacute" => 'ú',
        "ccedil" => 'ç',
        "ntilde" => 'ñ',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "Eacute" => 'É',
        "szlig" => 'ß',
        _ => return None,
    };
    Some(Some(c))
}

/// Position of `needle` in `hay` at or after `from`.
fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

/// Like [`find`], ignoring ASCII case.
fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle)).map(|p| from + p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Frame, ImageFormat, Rgba, RgbaImage};

    // ---- finding links ----

    #[test]
    fn finds_plain_links_and_trims_punctuation() {
        assert_eq!(
            find_urls("look https://example.com/a. and http://foo.org/b?x=1, wow!"),
            ["https://example.com/a", "http://foo.org/b?x=1"]
        );
        assert_eq!(find_urls("really?? https://example.com/!?"), ["https://example.com/"]);
        assert_eq!(find_urls("\"https://example.com/q\""), ["https://example.com/q"]);
        assert_eq!(find_urls("**https://example.com/bold**"), ["https://example.com/bold"]);
        assert_eq!(find_urls("HTTPS://Example.com/Up"), ["HTTPS://Example.com/Up"]);
    }

    #[test]
    fn keeps_balanced_parentheses() {
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(find_urls(wiki), [wiki]);
        assert_eq!(find_urls(&format!("(see {wiki})")), [wiki]);
        assert_eq!(find_urls(&format!("(see {wiki}).")), [wiki]);
        assert_eq!(find_urls("(https://example.com/x)"), ["https://example.com/x"]);
        assert_eq!(find_urls("[link](https://example.com/md)"), ["https://example.com/md"]);
    }

    #[test]
    fn angle_brackets_suppress_previews() {
        assert!(find_urls("no preview <https://example.com/> please").is_empty());
        assert_eq!(find_urls("<https://a.com/> https://b.com/"), ["https://b.com/"]);
        // Only that occurrence is suppressed.
        assert_eq!(find_urls("<https://a.com/> then https://a.com/"), ["https://a.com/"]);
    }

    #[test]
    fn at_most_three_without_duplicates() {
        let text = "https://a.com https://b.com https://a.com https://a.com#frag https://c.com https://d.com";
        assert_eq!(find_urls(text), ["https://a.com", "https://b.com", "https://c.com"]);
    }

    #[test]
    fn ignores_non_links_and_code() {
        assert!(find_urls("ftp://example.com mailto:me@x.com httpx://no").is_empty());
        assert!(find_urls("https:// nothing").is_empty());
        assert!(find_urls("xhttps://glued.com").is_empty());
        assert!(find_urls("`https://in-code.com` and ```\nhttps://in-block.com\n```").is_empty());
        assert_eq!(find_urls("`code` https://after.com"), ["https://after.com"]);
        assert_eq!(find_urls("ünïcode→https://example.com/ü"), ["https://example.com/ü"]);
        assert_eq!(find_urls("||https://spoiler.com||"), ["https://spoiler.com"]);
    }

    // ---- metadata ----

    fn base() -> Url {
        Url::parse("https://example.com/articles/one.html").unwrap()
    }

    fn meta(html: &str) -> PageMeta {
        parse_meta(html, &base(), Some("utf-8"), html.as_bytes())
    }

    #[test]
    fn reads_opengraph_tags() {
        let m = meta(
            r##"<!DOCTYPE html><html><head>
            <meta charset="utf-8">
            <title>Fallback title</title>
            <meta property="og:site_name" content="Example News">
            <meta property="og:title" content="Big &amp; Important &#39;News&#39; &quot;today&quot; &#x1F600;">
            <meta content='Single quoted description' property='og:description' />
            <meta property=og:type content=article>
            <meta property="og:image" content="/img/cover.jpg">
            <meta property="og:image:width" content="1200"><meta property="og:image:height" content=" 630 ">
            <meta property="og:video" content="https://example.com/v.mp4">
            <meta name="theme-color" content="#FF8800">
            </head><body><meta property="og:title" content="ignored, first wins"></body></html>"##,
        );
        assert_eq!(m.site_name.as_deref(), Some("Example News"));
        assert_eq!(m.title.as_deref(), Some("Big & Important 'News' \"today\" 😀"));
        assert_eq!(m.description.as_deref(), Some("Single quoted description"));
        assert_eq!(m.kind.as_deref(), Some("article"));
        assert_eq!(m.image.unwrap().as_str(), "https://example.com/img/cover.jpg");
        assert_eq!((m.image_width, m.image_height), (Some(1200), Some(630)));
        assert_eq!(m.color, Some(0xff8800));
        assert!(m.has_video);
        assert_eq!(m.charset.as_deref(), Some("utf-8"));
    }

    #[test]
    fn falls_back_to_title_and_description_tags() {
        let m = meta(
            "<html><HEAD><TITLE>\n  Plain   &lt;Page&gt;\n</TITLE>\
             <META NAME=\"Description\" CONTENT=\"Just a page\">\
             <meta name=\"twitter:image\" content=\"pic.png\"></HEAD></html>",
        );
        assert_eq!(m.title.as_deref(), Some("Plain <Page>"));
        assert_eq!(m.description.as_deref(), Some("Just a page"));
        assert_eq!(m.image.unwrap().as_str(), "https://example.com/articles/pic.png");
        assert_eq!(m.site_name, None);
        assert_eq!(m.color, None);
        assert!(!m.has_video);
    }

    #[test]
    fn missing_tags_and_tricky_markup() {
        assert_eq!(meta("<html><head></head><body>hi</body></html>"), PageMeta::default());
        assert_eq!(meta(""), PageMeta::default());
        let m = meta(
            r#"<head>
            <!-- <meta property="og:title" content="in a comment"> -->
            <script>var s = '<meta property="og:title" content="in a script">';</script>
            <style>/* <title>no</title> */</style>
            <meta property="og:title" content="">
            <meta property="og:description" content="He said &quot;hi&quot; > there" data-x='a"b'>
            <meta property="og:title" content="Real &unknown; & title">
            <base href="https://cdn.example.net/assets/">
            <meta property="og:image" content="//img.example.org/a.png?x=1&amp;y=2">
            <meta property="og:image" content="second.png">
            <meta name="theme-color" media="(prefers-color-scheme: dark)" content="rgba(255, 0, 16, 0.98)">
            <meta property="og:image:width" content="wide">
            </head>"#,
        );
        assert_eq!(m.title.as_deref(), Some("Real &unknown; & title"));
        assert_eq!(m.description.as_deref(), Some("He said \"hi\" > there"));
        assert_eq!(m.image.unwrap().as_str(), "https://img.example.org/a.png?x=1&y=2");
        assert_eq!(m.color, Some(0xff0010));
        assert_eq!(m.image_width, None);
        // Relative to <base href>.
        let m = meta(r#"<base href="https://cdn.example.net/assets/"><meta property="og:image" content="b.png">"#);
        assert_eq!(m.image.unwrap().as_str(), "https://cdn.example.net/assets/b.png");
        // Non-http picture URLs are dropped.
        assert_eq!(meta(r#"<meta property="og:image" content="data:image/png;base64,AAAA">"#).image, None);
    }

    #[test]
    fn long_text_is_trimmed() {
        let long = "word ".repeat(200);
        let m =
            meta(&format!(r#"<meta property="og:title" content="{long}"><meta name="description" content="{long}">"#));
        let title = m.title.unwrap();
        assert!(title.chars().count() <= MAX_TITLE && title.chars().count() > MAX_TITLE - 5);
        assert!(title.ends_with("word…"), "{title}");
        assert!(m.description.unwrap().chars().count() <= MAX_DESCRIPTION);
    }

    #[test]
    fn latin1_pages_are_decoded() {
        let raw: &[u8] = b"<meta charset=\"ISO-8859-1\"><title>Caf\xe9 \x93quoted\x94</title>";
        let html = decode_text(raw, None);
        let m = parse_meta(&html, &base(), None, raw);
        assert_eq!(m.title.as_deref(), Some("Café “quoted”"));
        let m = parse_meta(&decode_text(raw, Some("windows-1252")), &base(), Some("windows-1252"), raw);
        assert_eq!(m.title.as_deref(), Some("Café “quoted”"));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#fff"), Some(0xffffff));
        assert_eq!(parse_color("#1a2B3c"), Some(0x1a2b3c));
        assert_eq!(parse_color(" #f80 "), Some(0xff8800));
        assert_eq!(parse_color("#11223344"), Some(0x112233));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(0x010203));
        assert_eq!(parse_color("rgb(1 2 3 / 50%)"), Some(0x010203));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color("red"), None);
        assert_eq!(parse_color("rgb(300, 0, 0)"), None);
    }

    #[test]
    fn entities() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &#39;d&#39; &#x27;e&#X27; &quot;f&quot;"),
            "a & b <c> 'd' 'e' \"f\""
        );
        assert_eq!(decode_entities("Tom &amp Jerry & co; &bogus; &#xZZ; &"), "Tom &amp Jerry & co; &bogus; &#xZZ; &");
        assert_eq!(decode_entities("&hellip;&mdash;&#150;&shy;&nbsp;"), "…—–\u{a0}");
        assert_eq!(decode_entities("&#0;&#1114112;"), "\u{fffd}\u{fffd}");
    }

    // ---- addresses ----

    #[test]
    fn ip_classifier() {
        let cases = [
            ("8.8.8.8", true),
            ("1.1.1.1", true),
            ("93.184.216.34", true),
            ("100.63.255.255", true),
            ("100.128.0.1", true),
            ("172.15.255.255", true),
            ("172.32.0.1", true),
            ("0.0.0.0", false),
            ("0.1.2.3", false),
            ("10.0.0.1", false),
            ("100.64.0.1", false),
            ("100.100.100.100", false),
            ("100.127.255.254", false),
            ("127.0.0.1", false),
            ("127.255.255.255", false),
            ("169.254.169.254", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("192.0.0.8", false),
            ("192.0.2.1", false),
            ("192.88.99.1", false),
            ("192.168.1.1", false),
            ("198.18.0.1", false),
            ("198.19.255.255", false),
            ("198.51.100.7", false),
            ("203.0.113.9", false),
            ("224.0.0.1", false),
            ("239.255.255.250", false),
            ("240.0.0.1", false),
            ("255.255.255.255", false),
            ("2606:4700:4700::1111", true),
            ("2001:4860:4860::8888", true),
            ("2a00:1450:4001::200e", true),
            ("::", false),
            ("::1", false),
            ("::127.0.0.1", false),
            ("::ffff:127.0.0.1", false),
            ("::ffff:10.1.2.3", false),
            ("::ffff:192.168.0.1", false),
            ("::ffff:100.64.1.1", false),
            ("::ffff:169.254.169.254", false),
            ("::ffff:8.8.8.8", true),
            ("64:ff9b::8.8.8.8", true),
            ("64:ff9b::192.168.1.1", false),
            ("64:ff9b:1::1", false),
            ("2002:c0a8:0101::1", false),
            ("2002:0808:0808::1", true),
            ("fc00::1", false),
            ("fd12:3456:789a::1", false),
            ("fe80::1", false),
            ("febf::1", false),
            ("fec0::1", false),
            ("ff02::1", false),
            ("100::1", false),
            ("2001::1", false),
            ("2001:db8::1", false),
            ("3fff::1", false),
            ("3ffe::1", true),
        ];
        for (ip, public) in cases {
            assert_eq!(is_public_ip(ip.parse().unwrap()), public, "{ip}");
        }
    }

    // ---- fetching from a local server ----

    #[derive(Clone, Default)]
    struct Hits(Arc<AtomicUsize>);

    impl Hits {
        fn bump(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn gif(w: u32, h: u32, n: usize) -> Vec<u8> {
        let frames: Vec<Frame> = (0..n)
            .map(|i| {
                let img = RgbaImage::from_pixel(w, h, Rgba([(i * 40 % 256) as u8, 0, 0, 255]));
                Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(50, 1))
            })
            .collect();
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new_with_speed(&mut out, 30);
            enc.set_repeat(Repeat::Infinite).unwrap();
            enc.encode_frames(frames).unwrap();
        }
        out
    }

    fn html(body: impl Into<String>) -> Response {
        ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body.into()).into_response()
    }

    fn redirect(to: &str) -> Response {
        (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
    }

    /// Start the test site; returns its base URL (`http://127.0.0.1:port`) and a hit counter.
    async fn site() -> (String, Hits) {
        let hits = Hits::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let big_png = png(1200, 900);
        let small_gif = gif(64, 48, 3);
        let b = base.clone();
        let h = hits.clone();
        let app = Router::new()
            .route(
                "/article",
                get(|| async {
                    html(
                        r##"<html><head><title>T</title>
                        <meta property="og:site_name" content="Test Site">
                        <meta property="og:title" content="An article">
                        <meta property="og:description" content="About things &amp; stuff">
                        <meta property="og:image" content="/cover.png">
                        <meta name="theme-color" content="#336699">
                        </head><body>hello</body></html>"##,
                    )
                }),
            )
            .route(
                "/cover.png",
                get(move || {
                    let big_png = big_png.clone();
                    async move { ([(header::CONTENT_TYPE, "image/png")], big_png).into_response() }
                }),
            )
            .route(
                "/dance.gif",
                get(move || {
                    let small_gif = small_gif.clone();
                    async move { ([(header::CONTENT_TYPE, "image/gif")], small_gif).into_response() }
                }),
            )
            .route("/r1", get(|| async { redirect("/r2") }))
            .route("/r2", get(move || std::future::ready(redirect(&format!("{b}/r3")))))
            .route("/r3", get(|| async { (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "article")]).into_response() }))
            .route(
                "/hops/{n}",
                get(|axum::extract::Path(n): axum::extract::Path<u32>| async move {
                    if n == 0 { redirect("/article") } else { redirect(&format!("/hops/{}", n - 1)) }
                }),
            )
            .route("/loop/{n}", get(|axum::extract::Path(n): axum::extract::Path<u32>| async move { redirect(&format!("/loop/{}", n + 1)) }))
            .route("/to-private", get(|| async { redirect("http://127.0.0.2:9/secret") }))
            .route("/to-localhost", get(|| async { redirect("http://localhost:9/secret") }))
            .route(
                "/huge",
                get(|| async {
                    // Tags first, then a head that never ends.
                    let mut page = String::from(r#"<html><head><meta property="og:title" content="Huge page">"#);
                    page.push_str(&"<!-- padding padding padding -->\n".repeat(150_000));
                    html(page)
                }),
            )
            .route(
                "/huge-late",
                get(|| async {
                    let mut page = String::from("<html><head>");
                    page.push_str(&"<meta name=x content=y>\n".repeat(100_000));
                    page.push_str(r#"<meta property="og:title" content="Too late"></head>"#);
                    html(page)
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    html("<title>finally</title>")
                }),
            )
            .route("/json", get(|| async { ([(header::CONTENT_TYPE, "application/json")], "{\"a\":1}").into_response() }))
            .route("/pdf", get(|| async { ([(header::CONTENT_TYPE, "application/pdf")], "%PDF-1.4").into_response() }))
            .route("/empty", get(|| async { html("<html><head></head><body>nothing</body></html>") }))
            .route("/missing", get(|| async { (StatusCode::NOT_FOUND, "nope").into_response() }))
            .route(
                "/fake-image",
                get(|| async { ([(header::CONTENT_TYPE, "image/png")], "definitely not a png").into_response() }),
            )
            .route(
                "/bad-image-page",
                get(|| async {
                    html(r#"<meta property="og:title" content="Has a broken picture"><meta property="og:image" content="/fake-image">"#)
                }),
            )
            .route(
                "/private-image-page",
                get(|| async {
                    html(r#"<meta property="og:title" content="Sneaky"><meta property="og:image" content="http://192.168.1.1/x.png">"#)
                }),
            )
            .layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
                let h = h.clone();
                async move {
                    h.bump();
                    next.run(req).await
                }
            }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, hits)
    }

    fn test_fetcher() -> Fetcher {
        Fetcher::new_for_tests()
    }

    #[tokio::test]
    async fn html_page_with_opengraph_and_picture() {
        let (base, _) = site().await;
        let p = test_fetcher().fetch(&format!("{base}/article")).await.unwrap().expect("preview");
        assert_eq!(p.url, format!("{base}/article"));
        assert_eq!(p.site_name.as_deref(), Some("Test Site"));
        assert_eq!(p.title.as_deref(), Some("An article"));
        assert_eq!(p.description.as_deref(), Some("About things & stuff"));
        assert_eq!(p.color, Some(0x336699));
        assert!(!p.is_media);
        let img = p.image.as_ref().expect("picture");
        assert_eq!((img.width, img.height, img.mime.as_str()), (1200, 900, "image/png"));
        assert!(!img.animated);
        let thumb = image::load_from_memory(img.thumb.as_ref().expect("thumbnail")).unwrap();
        assert!(thumb.width() <= 480 && thumb.height() <= 360, "{}x{}", thumb.width(), thumb.height());
    }

    #[tokio::test]
    async fn direct_image_link_is_media() {
        let (base, _) = site().await;
        let f = test_fetcher();
        let p = f.fetch(&format!("{base}/cover.png")).await.unwrap().expect("preview");
        assert!(p.is_media);
        assert_eq!(p.title, None);
        assert_eq!(p.image.as_ref().unwrap().width, 1200);

        let p = f.fetch(&format!("{base}/dance.gif")).await.unwrap().expect("preview");
        let img = p.image.as_ref().unwrap();
        assert!(p.is_media && img.animated);
        assert_eq!((img.width, img.height, img.mime.as_str()), (64, 48, "image/gif"));
        assert!(img.thumb.is_none(), "small GIFs are kept as they are");

        // Labelled as a picture but isn't one.
        assert_eq!(f.fetch(&format!("{base}/fake-image")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn follows_redirect_chain() {
        let (base, _) = site().await;
        let f = test_fetcher();
        let p = f.fetch(&format!("{base}/r1")).await.unwrap().expect("preview");
        assert_eq!(p.url, format!("{base}/article"));
        assert_eq!(p.title.as_deref(), Some("An article"));

        // Five redirects are fine (/hops/4 is four hops plus one to /article), six are not.
        assert!(f.fetch(&format!("{base}/hops/4")).await.unwrap().is_some());
        let err = f.fetch(&format!("{base}/hops/5")).await.unwrap_err();
        assert!(err.contains("too many redirects"), "{err}");
        let err = f.fetch(&format!("{base}/loop/0")).await.unwrap_err();
        assert!(err.contains("too many redirects"), "{err}");
    }

    #[tokio::test]
    async fn oversized_pages_are_cut_off() {
        let (base, _) = site().await;
        let f = test_fetcher();
        // ~5 MB page: only the first 1 MB is read, which has the tags.
        let started = Instant::now();
        let p = f.fetch(&format!("{base}/huge")).await.unwrap().expect("preview");
        assert_eq!(p.title.as_deref(), Some("Huge page"));
        assert!(started.elapsed() < Duration::from_secs(5));
        // Tags after the cap aren't seen.
        assert_eq!(f.fetch(&format!("{base}/huge-late")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn slow_pages_time_out() {
        let (base, _) = site().await;
        let f = test_fetcher().with_limits(Limits { page_timeout: Duration::from_millis(300), ..Limits::default() });
        let started = Instant::now();
        let err = f.fetch(&format!("{base}/slow")).await.unwrap_err();
        assert!(err.contains("too long"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn other_types_and_errors() {
        let (base, _) = site().await;
        let f = test_fetcher();
        assert_eq!(f.fetch(&format!("{base}/json")).await.unwrap(), None);
        assert_eq!(f.fetch(&format!("{base}/pdf")).await.unwrap(), None);
        assert_eq!(f.fetch(&format!("{base}/empty")).await.unwrap(), None);
        let err = f.fetch(&format!("{base}/missing")).await.unwrap_err();
        assert!(err.contains("404"), "{err}");
        assert!(f.fetch("ftp://example.com/").await.is_err());
        assert!(f.fetch("not a url").await.is_err());
    }

    #[tokio::test]
    async fn broken_or_too_big_pictures_are_left_out() {
        let (base, _) = site().await;
        let p = test_fetcher().fetch(&format!("{base}/bad-image-page")).await.unwrap().expect("preview");
        assert_eq!(p.title.as_deref(), Some("Has a broken picture"));
        assert!(p.image.is_none());

        let f = test_fetcher().with_limits(Limits { max_image_bytes: 1_000, ..Limits::default() });
        let p = f.fetch(&format!("{base}/article")).await.unwrap().expect("preview");
        assert_eq!(p.title.as_deref(), Some("An article"));
        assert!(p.image.is_none());
        assert!(f.fetch(&format!("{base}/cover.png")).await.unwrap_err().contains("too big"));
    }

    #[tokio::test]
    async fn results_are_cached() {
        let (base, hits) = site().await;
        let f = test_fetcher();
        let a = f.fetch(&format!("{base}/article")).await.unwrap().unwrap();
        let after_first = hits.count();
        assert_eq!(after_first, 2, "page + picture");
        let b = f.fetch(&format!("{base}/article#section")).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(hits.count(), after_first);
        // Errors are cached too (briefly).
        f.fetch(&format!("{base}/missing")).await.unwrap_err();
        f.fetch(&format!("{base}/missing")).await.unwrap_err();
        assert_eq!(hits.count(), after_first + 1);
    }

    #[test]
    fn fetcher_can_be_shared_between_tasks() {
        fn send_sync<T: Send + Sync>(_: &T) {}
        fn send<T: Send>(_: T) {}
        let f = Fetcher::new_for_tests();
        send_sync(&f);
        send(f.fetch("http://example.com/")); // never polled; only checks the future is Send
    }

    #[test]
    fn cache_evicts_oldest() {
        let mut c = Cache::default();
        for i in 0..CACHE_ENTRIES + 10 {
            c.put(format!("k{i}"), &Ok(None));
        }
        assert_eq!(c.map.len(), CACHE_ENTRIES);
        assert!(c.get("k0").is_none());
        assert!(c.get(&format!("k{}", CACHE_ENTRIES + 9)).is_some());
    }

    // ---- strict mode ----

    #[tokio::test]
    async fn strict_mode_refuses_local_addresses() {
        let (base, hits) = site().await;
        let f = Fetcher::new();
        for url in [
            format!("{base}/article"),
            "http://localhost/".to_string(),
            format!("http://localhost:{}/article", base.rsplit(':').next().unwrap()),
            "http://[::1]/".into(),
            "http://[::ffff:127.0.0.1]/".into(),
            "http://2130706433/".into(),
            "http://0x7f.1/".into(),
            "http://192.168.1.1/".into(),
            "http://100.100.100.100/".into(),
            "http://169.254.169.254/latest/meta-data/".into(),
            "http://0.0.0.0/".into(),
            "http://user:pw@example.com/".into(),
        ] {
            let err = f.fetch(&url).await.unwrap_err();
            assert!(err.contains("aren't previewed"), "{url}: {err}");
        }
        assert_eq!(hits.count(), 0, "nothing reached the local server");
    }

    #[tokio::test]
    async fn strict_mode_refuses_redirects_to_local_addresses() {
        let (base, hits) = site().await;
        let mut f = Fetcher::new();
        // Let the test server itself through, but nothing else local.
        f.test_exempt_ip = Some("127.0.0.1".parse().unwrap());

        let err = f.fetch(&format!("{base}/to-private")).await.unwrap_err();
        assert!(err.contains("127.0.0.2") && err.contains("aren't previewed"), "{err}");
        let err = f.fetch(&format!("{base}/to-localhost")).await.unwrap_err();
        assert!(err.contains("localhost") && err.contains("aren't previewed"), "{err}");
        // og:image pointing into the LAN is dropped; the card still shows.
        let p = f.fetch(&format!("{base}/private-image-page")).await.unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Sneaky"));
        assert!(p.image.is_none());
        assert_eq!(hits.count(), 3);
    }
}
