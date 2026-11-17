//! The web app, embedded at build time from the directory in `FEIYI_APP_DIST`
//! (the `app` feature). Served for every path no other route claims.

use axum::http::{header, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

static DIST: Dir = include_dir!("$FEIYI_APP_DIST");

pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(decoded) = percent_decode(uri.path().trim_start_matches('/')) else { return StatusCode::NOT_FOUND.into_response() };
    if decoded.contains("..") || decoded.contains(['\0', '\\']) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if decoded.is_empty() { "index.html" } else { decoded.as_str() };
    let is_route = !path.rsplit('/').next().unwrap_or("").contains('.');
    let file = DIST.get_file(path).or_else(|| if is_route { DIST.get_file("index.html") } else { None });
    let Some(file) = file else { return StatusCode::NOT_FOUND.into_response() };
    // Expo content-hashes everything under these; the rest is revalidated.
    let hashed = path.starts_with("_expo/static/") || path.starts_with("assets/");
    let cache = if hashed { "public, max-age=31536000, immutable" } else { "no-cache" };
    let ext = file.path().extension().and_then(|e| e.to_str()).unwrap_or("");
    let headers = [(header::CONTENT_TYPE, content_type(ext)), (header::CACHE_CONTROL, cache), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")];
    (headers, file.contents()).into_response()
}

fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "otf" => "font/otf",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "avif" => "image/avif",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
