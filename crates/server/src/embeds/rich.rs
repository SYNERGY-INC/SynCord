//! Richer previews for YouTube, X/Twitter and Spotify links. These sites either hide
//! the details from plain page fetches or put them somewhere OpenGraph doesn't, so they're
//! asked through their public oEmbed (and, for posts, fxtwitter) APIs instead. Every
//! request still goes through the guarded fetcher in the parent module.

use reqwest::Url;
use serde_json::Value;

use super::{Fetcher, MAX_DESCRIPTION, MAX_TITLE, Page, Preview, clean, decode_entities, read_capped};

/// Where each service is asked. Tests point these at a local server.
#[derive(Debug, Clone)]
pub struct Providers {
    /// Serves `/oembed` and `/watch?v=<id>`.
    pub youtube: String,
    /// Serves `/<user>/status/<id>` as JSON.
    pub fxtwitter: String,
    /// Serves `/oembed?url=...`.
    pub twitter_oembed: String,
    /// Serves `/oembed` and `/<kind>/<id>`.
    pub spotify: String,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            youtube: "https://www.youtube.com".into(),
            fxtwitter: "https://api.fxtwitter.com".into(),
            twitter_oembed: "https://publish.twitter.com".into(),
            spotify: "https://open.spotify.com".into(),
        }
    }
}

impl Providers {
    /// Every service under one test server: `<base>/youtube`, `<base>/fxtwitter`, ...
    pub fn all_at(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            youtube: format!("{base}/youtube"),
            fxtwitter: format!("{base}/fxtwitter"),
            twitter_oembed: format!("{base}/twitter"),
            spotify: format!("{base}/spotify"),
        }
    }
}

/// A link to something one of the services knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    YouTube { id: String },
    Post { user: String, id: String },
    Spotify { kind: String, id: String },
}

/// The extra details of a rich preview (see [`celerity_proto::Embed`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RichMeta {
    pub provider: &'static str,
    pub author: Option<String>,
    pub author_url: Option<String>,
    pub duration_secs: Option<u32>,
    pub embed_id: Option<String>,
}

fn host_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

fn valid_youtube_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Which service a link belongs to, if any.
pub fn detect(url: &Url) -> Option<Link> {
    let host = host_of(url);
    let segments: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    match host.as_str() {
        "youtu.be" => {
            let id = segments.first()?;
            valid_youtube_id(id).then(|| Link::YouTube { id: id.to_string() })
        }
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            let id = match segments.as_slice() {
                ["watch"] => url.query_pairs().find(|(k, _)| k == "v").map(|(_, v)| v.into_owned())?,
                ["shorts" | "embed" | "live" | "v", id, ..] => id.to_string(),
                _ => return None,
            };
            valid_youtube_id(&id).then_some(Link::YouTube { id })
        }
        "twitter.com" | "mobile.twitter.com" | "x.com" | "fxtwitter.com" | "vxtwitter.com" | "fixupx.com" => {
            match segments.as_slice() {
                [user, "status", id, ..]
                    if (1..=15).contains(&user.len())
                        && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && !id.is_empty()
                        && id.len() <= 20
                        && id.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    Some(Link::Post { user: user.to_string(), id: id.to_string() })
                }
                _ => None,
            }
        }
        "open.spotify.com" => {
            let rest = match segments.first() {
                Some(s) if s.starts_with("intl-") => &segments[1..],
                _ => &segments[..],
            };
            match rest {
                [kind @ ("track" | "album" | "playlist" | "episode" | "show" | "artist"), id, ..]
                    if !id.is_empty() && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_alphanumeric()) =>
                {
                    Some(Link::Spotify { kind: kind.to_string(), id: id.to_string() })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// `PT1H2M3S` (ISO 8601, as YouTube writes it) in seconds.
pub fn parse_iso_duration(s: &str) -> Option<u32> {
    let rest = s.trim().strip_prefix("PT").or_else(|| s.trim().strip_prefix("pt"))?;
    let (mut total, mut num) = (0u32, String::new());
    for c in rest.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u32 = num.parse().ok()?;
        num.clear();
        total = total.checked_add(match c.to_ascii_uppercase() {
            'H' => n.checked_mul(3600)?,
            'M' => n.checked_mul(60)?,
            'S' => n,
            _ => return None,
        })?;
    }
    num.is_empty().then_some(total)
}

fn text(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn http_url(s: Option<String>) -> Option<Url> {
    s.and_then(|u| Url::parse(&u).ok()).filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// A post's author, link to the author, text and picture.
pub type PostDetails = (String, Option<String>, Option<String>, Option<String>);

/// What a post says, from fxtwitter's JSON.
pub fn parse_fxtwitter(v: &Value) -> Option<PostDetails> {
    let tweet = v.get("tweet")?;
    let name = text(tweet, &["author", "name"]);
    let handle = text(tweet, &["author", "screen_name"])?;
    let author = match name {
        Some(n) => format!("{n} (@{handle})"),
        None => format!("@{handle}"),
    };
    let body = text(tweet, &["text"]);
    let media = tweet.get("media");
    let image = media
        .and_then(|m| m.get("photos"))
        .and_then(|p| p.get(0))
        .and_then(|p| text(p, &["url"]))
        .or_else(|| {
            media.and_then(|m| m.get("videos")).and_then(|p| p.get(0)).and_then(|p| text(p, &["thumbnail_url"]))
        })
        .or_else(|| media.and_then(|m| m.get("mosaic")).and_then(|m| text(m, &["formats", "jpeg"])));
    Some((author, Some(format!("https://x.com/{handle}")), body, image))
}

/// The post's text from Twitter's oEmbed HTML (`<blockquote><p>text</p>&mdash; ...`).
pub fn oembed_post_text(html: &str) -> Option<String> {
    let start = html.find("<p")?;
    let open_end = start + html[start..].find('>')? + 1;
    let end = open_end + html[open_end..].find("</p>")?;
    let inner = html[open_end..end].replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");
    let mut out = String::new();
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = decode_entities(out.trim());
    (!out.is_empty()).then_some(out)
}

/// The artist from Spotify's description, "Listen to X on Spotify. Artist · Song · 2019."
pub fn spotify_artist(description: &str) -> Option<String> {
    let after = match description.split_once(" on Spotify. ") {
        Some((_, rest)) => rest,
        None => description,
    };
    let (artist, _) = after.split_once(" · ")?;
    let artist = artist.trim();
    (!artist.is_empty()).then(|| artist.to_string())
}

impl Fetcher {
    async fn get_json(&self, url: Url) -> Result<Value, String> {
        let fut = async {
            let (_, mut resp) = self.get(url, "application/json").await?;
            let bytes = read_capped(&mut resp, 512 * 1024).await?;
            serde_json::from_slice::<Value>(&bytes).map_err(|_| "the site sent something unreadable".to_string())
        };
        tokio::time::timeout(self.limits.page_timeout, fut)
            .await
            .map_err(|_| "the site took too long to answer".to_string())?
    }

    async fn page_meta(&self, url: Url) -> Option<Box<super::PageMeta>> {
        match tokio::time::timeout(self.limits.page_timeout, self.load_page(url)).await {
            Ok(Ok(Page::Html { meta, .. })) => Some(meta),
            _ => None,
        }
    }

    fn parse_base(base: &str, path: &str, query: &[(&str, &str)]) -> Result<Url, String> {
        Url::parse_with_params(&format!("{}{path}", base.trim_end_matches('/')), query)
            .map_err(|_| "bad provider address".to_string())
    }

    /// A rich preview for a link one of the services knows about. `Err` (or `Ok(None)`)
    /// means the ordinary page preview should be tried instead.
    pub(super) async fn fetch_rich(&self, link: &Link, original: &Url) -> Result<Option<Preview>, String> {
        let p = &self.providers;
        let (title, description, image, meta, site_name, color) = match link {
            Link::YouTube { id } => {
                let watch = format!("https://www.youtube.com/watch?v={id}");
                let oembed = self
                    .get_json(Self::parse_base(&p.youtube, "/oembed", &[("format", "json"), ("url", &watch)])?)
                    .await;
                let page = self.page_meta(Self::parse_base(&p.youtube, "/watch", &[("v", id)])?).await;
                let oembed = oembed.ok();
                if oembed.is_none() && page.is_none() {
                    return Err("YouTube didn't answer".into());
                }
                let o = oembed.as_ref();
                let title = o.and_then(|o| text(o, &["title"])).or_else(|| page.as_ref().and_then(|m| m.title.clone()));
                let image = page
                    .as_ref()
                    .and_then(|m| m.image.clone())
                    .or_else(|| http_url(o.and_then(|o| text(o, &["thumbnail_url"]))));
                let meta = RichMeta {
                    provider: "youtube",
                    author: o.and_then(|o| text(o, &["author_name"])),
                    author_url: o.and_then(|o| text(o, &["author_url"])),
                    duration_secs: page.as_ref().and_then(|m| m.duration_secs),
                    embed_id: Some(id.clone()),
                };
                (title, page.as_ref().and_then(|m| m.description.clone()), image, meta, "YouTube", 0xFF0000)
            }
            Link::Post { user, id } => {
                let fx = self.get_json(Self::parse_base(&p.fxtwitter, &format!("/{user}/status/{id}"), &[])?).await;
                let (author, author_url, body, image) = match fx.ok().as_ref().and_then(parse_fxtwitter) {
                    Some((a, u, b, i)) => (a, u, b, http_url(i)),
                    None => {
                        let post = format!("https://twitter.com/{user}/status/{id}");
                        let o = self
                            .get_json(Self::parse_base(
                                &p.twitter_oembed,
                                "/oembed",
                                &[("url", &post), ("omit_script", "1")],
                            )?)
                            .await?;
                        let name = text(&o, &["author_name"]).unwrap_or_else(|| user.clone());
                        let body = text(&o, &["html"]).and_then(|h| oembed_post_text(&h));
                        (format!("{name} (@{user})"), text(&o, &["author_url"]), body, None)
                    }
                };
                let meta = RichMeta {
                    provider: "twitter",
                    author: Some(author.clone()),
                    author_url,
                    duration_secs: None,
                    embed_id: Some(id.clone()),
                };
                // Older apps only show the title and description: the author and the post.
                (Some(author), body, image, meta, "X", 0x1D9BF0)
            }
            Link::Spotify { kind, id } => {
                let canonical = format!("https://open.spotify.com/{kind}/{id}");
                let oembed = self.get_json(Self::parse_base(&p.spotify, "/oembed", &[("url", &canonical)])?).await;
                let page = self.page_meta(Self::parse_base(&p.spotify, &format!("/{kind}/{id}"), &[])?).await;
                let oembed = oembed.ok();
                if oembed.is_none() && page.is_none() {
                    return Err("Spotify didn't answer".into());
                }
                let o = oembed.as_ref();
                let title = o.and_then(|o| text(o, &["title"])).or_else(|| page.as_ref().and_then(|m| m.title.clone()));
                let artist = page
                    .as_ref()
                    .and_then(|m| m.musician.clone().or_else(|| m.description.as_deref().and_then(spotify_artist)));
                let image = http_url(o.and_then(|o| text(o, &["thumbnail_url"])))
                    .or_else(|| page.as_ref().and_then(|m| m.image.clone()));
                let meta = RichMeta {
                    provider: "spotify",
                    author: artist,
                    author_url: None,
                    duration_secs: page.as_ref().and_then(|m| m.duration_secs),
                    embed_id: Some(format!("{kind}/{id}")),
                };
                (title, None, image, meta, "Spotify", 0x1DB954)
            }
        };
        let image = match image {
            Some(url) => match tokio::time::timeout(self.limits.image_timeout, self.load_image(url)).await {
                Ok(Ok(bytes)) => self.check_image(bytes).await,
                _ => None,
            },
            None => None,
        };
        Ok(Some(Preview {
            url: original.to_string(),
            site_name: Some(site_name.into()),
            title: title.and_then(|t| clean(&t, MAX_TITLE)),
            description: description.and_then(|d| clean(&d, MAX_DESCRIPTION)),
            color: Some(color),
            image,
            kind: None,
            has_video: matches!(link, Link::YouTube { .. }),
            is_media: false,
            rich: Some(meta),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn recognises_links() {
        let yt = |id: &str| Some(Link::YouTube { id: id.into() });
        assert_eq!(detect(&url("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10")), yt("dQw4w9WgXcQ"));
        assert_eq!(detect(&url("https://youtu.be/dQw4w9WgXcQ?si=x")), yt("dQw4w9WgXcQ"));
        assert_eq!(detect(&url("https://m.youtube.com/shorts/abcdefghijk")), yt("abcdefghijk"));
        assert_eq!(detect(&url("https://www.youtube.com/watch?v=short")), None);
        assert_eq!(detect(&url("https://www.youtube.com/@channel")), None);
        assert_eq!(
            detect(&url("https://x.com/rustlang/status/1234567890?s=20")),
            Some(Link::Post { user: "rustlang".into(), id: "1234567890".into() })
        );
        assert_eq!(detect(&url("https://twitter.com/rustlang")), None);
        assert_eq!(
            detect(&url("https://open.spotify.com/intl-de/track/4uLU6hMCjMI75M1A2tKUQC?si=1")),
            Some(Link::Spotify { kind: "track".into(), id: "4uLU6hMCjMI75M1A2tKUQC".into() })
        );
        assert_eq!(detect(&url("https://open.spotify.com/user/x")), None);
        assert_eq!(detect(&url("https://example.com/watch?v=dQw4w9WgXcQ")), None);
    }

    #[test]
    fn parses_details() {
        assert_eq!(parse_iso_duration("PT4M13S"), Some(253));
        assert_eq!(parse_iso_duration("PT1H0M2S"), Some(3602));
        assert_eq!(parse_iso_duration("PT"), Some(0));
        assert_eq!(parse_iso_duration("4:13"), None);
        assert_eq!(spotify_artist("Listen to Song on Spotify. The Band · Song · 2019."), Some("The Band".into()));
        assert_eq!(spotify_artist("Nothing useful"), None);
        let html = r#"<blockquote class="twitter-tweet"><p lang="en" dir="ltr">Hello &amp; welcome<br>to <a href="x">#rust</a></p>&mdash; Rust (@rustlang)</blockquote>"#;
        assert_eq!(oembed_post_text(html).as_deref(), Some("Hello & welcome\nto #rust"));
        let fx: Value = serde_json::from_str(
            r#"{"tweet":{"text":"hi there","author":{"name":"Rust","screen_name":"rustlang"},
                "media":{"photos":[{"url":"https://pbs.twimg.com/a.jpg"}]}}}"#,
        )
        .unwrap();
        let (author, link, body, image) = parse_fxtwitter(&fx).unwrap();
        assert_eq!(author, "Rust (@rustlang)");
        assert_eq!(link.as_deref(), Some("https://x.com/rustlang"));
        assert_eq!(body.as_deref(), Some("hi there"));
        assert_eq!(image.as_deref(), Some("https://pbs.twimg.com/a.jpg"));
    }
}
