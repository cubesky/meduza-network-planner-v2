//! Prebuilt UI assets (`/usr/share/meduza-np/dashboard/www`): hashed files under `assets/` are immutable, `index.html`
//! is revalidated so an upgrade never pairs a new backend with a stale cached UI. `.gz` siblings produced at build
//! time are served when the browser accepts gzip. Unknown paths fall back to `index.html` (hash routing).

use std::path::{Component, Path};

use hyper::header::{HeaderMap, ACCEPT_ENCODING};

use crate::server::{content_type_for, file_response, not_found, Resp};

pub fn serve(www: &Path, path: &str, headers: &HeaderMap) -> Resp {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let rel_path = Path::new(rel);
    if !rel_path.components().all(|c| matches!(c, Component::Normal(_))) {
        return not_found();
    }
    let gzip_ok = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|v| v.contains("gzip")).unwrap_or(false);
    let try_file = |rel: &str| -> Option<Resp> {
        let full = www.join(rel);
        let immutable = rel.starts_with("assets/");
        let ctype = content_type_for(rel);
        if gzip_ok {
            if let Ok(b) = std::fs::read(format!("{}.gz", full.display())) {
                return Some(file_response(b, ctype, true, immutable));
            }
        }
        std::fs::read(&full).ok().map(|b| file_response(b, ctype, false, immutable))
    };
    if let Some(r) = try_file(rel) {
        return r;
    }
    if rel.starts_with("assets/") || rel.contains('.') {
        return not_found();
    }
    try_file("index.html").unwrap_or_else(not_found)
}
