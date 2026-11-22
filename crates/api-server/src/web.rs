//! Serves the built SPA (`web/build`) for every non-`/api` path, falling
//! back to `index.html` for client-side routes. Public: the login page must
//! load without a session.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use tagmail_api_core::api::WEB_APP_CSP;

pub const INDEX: &str = "index.html";
/// SvelteKit's content-hashed assets.
const IMMUTABLE_PREFIX: &str = "_app/immutable/";

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// The file under `root` for a decoded URL path, or `None` if the path
/// tries to leave `root`.
fn resolve(root: &Path, path: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        if segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment.contains('\0')
            || segment.contains(':')
        {
            return None;
        }
        out.push(segment);
    }
    Some(out)
}

pub struct WebDir {
    root: PathBuf,
}

impl WebDir {
    pub fn new(root: PathBuf) -> WebDir {
        WebDir { root }
    }

    pub async fn serve(&self, method: &Method, path: &str) -> HttpResponse {
        if !matches!(*method, Method::GET | Method::HEAD) {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let Some(mut file) = resolve(&self.root, path) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let relative = path.trim_start_matches('/');
        if !tokio::fs::metadata(&file).await.is_ok_and(|m| m.is_file()) {
            let last = relative.rsplit('/').next().unwrap_or("");
            if last.contains('.') {
                return StatusCode::NOT_FOUND.into_response();
            }
            file = self.root.join(INDEX);
        }
        let bytes = match tokio::fs::read(&file).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(file = %file.display(), error = %e, "web asset unreadable");
                return StatusCode::NOT_FOUND.into_response();
            }
        };
        let cache = if relative.starts_with(IMMUTABLE_PREFIX) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let body = if *method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(bytes)
        };
        let mut resp = HttpResponse::new(body);
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(&file)),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(WEB_APP_CSP),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_leave_the_root() {
        let root = Path::new("/srv/web");
        assert_eq!(
            resolve(root, "/_app/x.js").unwrap(),
            Path::new("/srv/web/_app/x.js")
        );
        assert_eq!(resolve(root, "/").unwrap(), root);
        for bad in ["/../etc/passwd", "/a/../../b", "/a\\..\\b", "/C:/x"] {
            assert!(resolve(root, bad).is_none(), "{bad}");
        }
    }
}
