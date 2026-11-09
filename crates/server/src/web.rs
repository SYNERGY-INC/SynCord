//! The web app (see `web/README.md`): built into the server, served at `/`, so phones
//! can open the server's address in a browser and add Celerity to their home screen.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

mod files {
    include!(concat!(env!("OUT_DIR"), "/web_files.rs"));
}

fn find(path: &str) -> Option<&'static [u8]> {
    files::FILES.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn serve(path: &str, bytes: &'static [u8]) -> Response {
    let mut resp = Body::from(bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime(path)));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    // Built files have a content hash in their name and never change; the page and the
    // service worker must always be fetched fresh so updates arrive.
    let fixed = path.starts_with("/assets/");
    let cache = if fixed { "public, max-age=31536000, immutable" } else { "no-cache" };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

/// `GET /`: the web app for browsers. Anything else (scripts, installers and health checks
/// that look for "Celerity server is running") gets the plain reply it always got.
pub async fn index(headers: axum::http::HeaderMap) -> Response {
    let browser = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    match find("/index.html").filter(|_| browser) {
        Some(bytes) => serve("/index.html", bytes),
        None => "Celerity server is running.\n".into_response(),
    }
}

/// Any other `GET`: one of the web app's files.
pub async fn asset(uri: Uri) -> Response {
    let path = uri.path();
    match find(path).filter(|_| path != "/index.html") {
        Some(bytes) => serve(path, bytes),
        None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_types() {
        assert_eq!(super::mime("/assets/app-1a2b.js"), "text/javascript; charset=utf-8");
        assert_eq!(super::mime("/assets/opus-3c.wasm"), "application/wasm");
        assert_eq!(super::mime("/manifest.webmanifest"), "application/manifest+json");
    }
}
