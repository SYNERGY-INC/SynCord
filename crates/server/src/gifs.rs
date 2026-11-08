//! GIF search through GIPHY or KLIPY. The API key stays on the server, so
//! friends don't need one; picking a GIF makes the server download it and post
//! it as a normal attachment.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use syncord_proto::{GifResult, UserId};

use crate::Config;

const RESULTS: usize = 30;
/// Largest GIF the server will fetch for a message.
pub const MAX_GIF_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Giphy,
    Klipy,
}

pub struct GifSearch {
    kind: Kind,
    key: String,
    base: String,
    http: reqwest::Client,
    /// GIF id -> download address, from recent results. Only these can be sent.
    full_urls: Mutex<HashMap<String, (String, String)>>,
}

/// One parsed result plus the address of the full-size GIF.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub result: GifResult,
    pub full_url: String,
}

impl GifSearch {
    /// Set up from the server settings, if a key is configured (GIPHY wins if both are).
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let (kind, key) = if !cfg.giphy_api_key.trim().is_empty() {
            (Kind::Giphy, cfg.giphy_api_key.trim().to_string())
        } else if !cfg.klipy_api_key.trim().is_empty() {
            (Kind::Klipy, cfg.klipy_api_key.trim().to_string())
        } else {
            return None;
        };
        let base = if !cfg.gif_api_base.is_empty() {
            cfg.gif_api_base.trim_end_matches('/').to_string()
        } else {
            match kind {
                Kind::Giphy => "https://api.giphy.com/v1/gifs".into(),
                Kind::Klipy => format!("https://api.klipy.com/api/v1/{key}/gifs"),
            }
        };
        Some(Self { kind, key, base, http: syncord_update::client(), full_urls: Mutex::new(HashMap::new()) })
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            Kind::Giphy => "GIPHY",
            Kind::Klipy => "KLIPY",
        }
    }

    /// Search (or trending, for an empty query).
    pub async fn search(&self, query: &str, user: UserId) -> Result<Vec<GifResult>, String> {
        let query = query.trim();
        let mut url = reqwest::Url::parse(&if query.is_empty() {
            format!("{}/trending", self.base)
        } else {
            format!("{}/search", self.base)
        })
        .map_err(|e| e.to_string())?;
        {
            let mut q = url.query_pairs_mut();
            match self.kind {
                Kind::Giphy => {
                    q.append_pair("api_key", &self.key)
                        .append_pair("limit", &RESULTS.to_string())
                        .append_pair("rating", "pg-13");
                }
                Kind::Klipy => {
                    q.append_pair("per_page", &RESULTS.to_string())
                        .append_pair("page", "1")
                        .append_pair("customer_id", &format!("syncord-{user}"))
                        .append_pair("content_filter", "medium");
                }
            }
            if !query.is_empty() {
                q.append_pair("q", &query.chars().take(50).collect::<String>());
            }
        }
        let resp = self.http.get(url).send().await.map_err(|e| format!("GIF search failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GIF search failed ({} answered {})", self.name(), resp.status().as_u16()));
        }
        let json: Value = resp.json().await.map_err(|e| format!("GIF search failed: {e}"))?;
        let found = match self.kind {
            Kind::Giphy => parse_giphy(&json),
            Kind::Klipy => parse_klipy(&json),
        };
        let mut cache = self.full_urls.lock().unwrap();
        if cache.len() > 5000 {
            cache.clear();
        }
        for f in &found {
            cache.insert(f.result.id.clone(), (f.full_url.clone(), f.result.title.clone()));
        }
        Ok(found.into_iter().map(|f| f.result).collect())
    }

    /// Download address and title of a GIF from recent results.
    pub fn lookup(&self, id: &str) -> Option<(String, String)> {
        self.full_urls.lock().unwrap().get(id).cloned()
    }

    /// Fetch a GIF for sending, refusing anything too big or not a GIF.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self.http.get(url).send().await.map_err(|e| format!("couldn't fetch the GIF: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("couldn't fetch the GIF (error {})", resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > MAX_GIF_BYTES) {
            return Err("that GIF is too big".into());
        }
        let bytes = resp.bytes().await.map_err(|e| format!("couldn't fetch the GIF: {e}"))?;
        if bytes.len() as u64 > MAX_GIF_BYTES {
            return Err("that GIF is too big".into());
        }
        if !matches!(image::guess_format(&bytes), Ok(image::ImageFormat::Gif | image::ImageFormat::WebP)) {
            return Err("that doesn't look like a GIF".into());
        }
        Ok(bytes.to_vec())
    }
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str().filter(|s| !s.is_empty())
}

fn num_at(v: &Value, path: &[&str]) -> u32 {
    let Some(v) = path.iter().try_fold(v, |v, k| v.get(k)) else { return 0 };
    // GIPHY sends sizes as strings, KLIPY as numbers.
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0) as u32
}

fn id_of(v: &Value) -> Option<String> {
    match v.get("id")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// GIPHY: `{"data": [{"id", "title", "images": {"fixed_width_small": {"url", "width", "height"}, ...}}]}`.
pub fn parse_giphy(json: &Value) -> Vec<Found> {
    let Some(items) = json.get("data").and_then(Value::as_array) else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let id = id_of(item)?;
            let images = item.get("images")?;
            let preview = ["fixed_width_small", "fixed_width", "fixed_height_small"]
                .into_iter()
                .find(|k| str_at(images, &[k, "url"]).is_some())?;
            let still = ["fixed_width_small_still", "fixed_width_still", "original_still"]
                .into_iter()
                .find_map(|k| str_at(images, &[k, "url"]))
                .unwrap_or_default();
            let full =
                ["downsized_medium", "downsized", "original"].into_iter().find_map(|k| str_at(images, &[k, "url"]))?;
            Some(Found {
                result: GifResult {
                    id,
                    title: str_at(item, &["title"]).unwrap_or("GIF").to_string(),
                    preview_url: str_at(images, &[preview, "url"])?.to_string(),
                    still_url: still.to_string(),
                    width: num_at(images, &[preview, "width"]),
                    height: num_at(images, &[preview, "height"]),
                },
                full_url: full.to_string(),
            })
        })
        .collect()
}

/// KLIPY: `{"data": {"data": [{"id", "title", "file": {"hd"|"md"|"sm"|"xs": {"gif"|"webp"|"jpg": {"url", ...}}}}]}}`
/// (some responses put the list straight under `data`).
pub fn parse_klipy(json: &Value) -> Vec<Found> {
    let data = json.get("data").unwrap_or(&Value::Null);
    let items = data.as_array().or_else(|| data.get("data").and_then(Value::as_array));
    let Some(items) = items else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let id = id_of(item)?;
            let file = item.get("file")?;
            let pick = |options: &[(&'static str, &'static str)]| {
                options.iter().find(|(size, fmt)| str_at(file, &[size, fmt, "url"]).is_some()).copied()
            };
            let (psize, pfmt) = pick(&[("sm", "gif"), ("xs", "gif"), ("md", "gif"), ("sm", "webp")])?;
            let still = pick(&[("xs", "jpg"), ("sm", "jpg"), ("md", "jpg")])
                .and_then(|(s, f)| str_at(file, &[s, f, "url"]))
                .unwrap_or_default();
            let full = pick(&[("hd", "gif"), ("md", "gif"), ("sm", "gif")])
                .and_then(|(s, f)| str_at(file, &[s, f, "url"]))
                .or_else(|| str_at(file, &["gif", "url"]))?;
            Some(Found {
                result: GifResult {
                    id,
                    title: str_at(item, &["title"]).unwrap_or("GIF").to_string(),
                    preview_url: str_at(file, &[psize, pfmt, "url"])?.to_string(),
                    still_url: still.to_string(),
                    width: num_at(file, &[psize, pfmt, "width"]),
                    height: num_at(file, &[psize, pfmt, "height"]),
                },
                full_url: full.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn giphy_results() {
        let j = json!({"data": [{
            "id": "abc123", "title": "Happy Dance",
            "images": {
                "fixed_width_small": {"url": "https://media.giphy.com/s.gif", "width": "100", "height": "75"},
                "fixed_width_small_still": {"url": "https://media.giphy.com/s_s.gif"},
                "downsized_medium": {"url": "https://media.giphy.com/d.gif"},
                "original": {"url": "https://media.giphy.com/o.gif"}
            }
        }, {"id": "broken"}], "pagination": {}, "meta": {}});
        let r = parse_giphy(&j);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].result.id, "abc123");
        assert_eq!(r[0].result.preview_url, "https://media.giphy.com/s.gif");
        assert_eq!(r[0].result.still_url, "https://media.giphy.com/s_s.gif");
        assert_eq!((r[0].result.width, r[0].result.height), (100, 75));
        assert_eq!(r[0].full_url, "https://media.giphy.com/d.gif");
    }

    #[test]
    fn klipy_results_nested_and_flat() {
        let item = json!({
            "id": 2525964843568523u64, "title": "domino",
            "file": {
                "hd": {"gif": {"url": "https://static.klipy.com/hd.gif", "width": 498, "height": 280}},
                "sm": {"gif": {"url": "https://static.klipy.com/sm.gif", "width": 220, "height": 124}},
                "xs": {"jpg": {"url": "https://static.klipy.com/xs.jpg"}}
            }
        });
        for j in [
            json!({"result": true, "data": {"data": [item.clone()], "has_next": true}}),
            json!({"data": [item.clone()]}),
        ] {
            let r = parse_klipy(&j);
            assert_eq!(r.len(), 1, "{j}");
            assert_eq!(r[0].result.id, "2525964843568523");
            assert_eq!(r[0].result.preview_url, "https://static.klipy.com/sm.gif");
            assert_eq!(r[0].result.still_url, "https://static.klipy.com/xs.jpg");
            assert_eq!(r[0].result.width, 220);
            assert_eq!(r[0].full_url, "https://static.klipy.com/hd.gif");
        }
        assert!(parse_klipy(&json!({"error": "bad key"})).is_empty());
    }
}
