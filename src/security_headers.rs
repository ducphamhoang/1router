//! Response hardening headers for every route (SEC-08).
//!
//! `SameSite=Strict` doesn't look at the port, so another web app on the
//! same host could frame `/ui/*` with the admin's session and clickjack
//! it; `frame-ancestors 'none'` + `X-Frame-Options: DENY` stop that. The CSP
//! is a backstop against any future XSS in the admin UI: scripts only from
//! our own origin (the Vite bundle), no plugins, no `<base>` hijack.
//! `style-src` allows inline styles because React/dnd-kit set them.

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

pub async fn security_headers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    // Admin responses carry secrets (export, one-time user keys): never cache.
    // The UI shell must revalidate so a new build's asset hashes are picked up;
    // hashed assets under /ui/assets/ are immutable and keep default caching.
    let cache = if path.starts_with("/admin") {
        Some("no-store")
    } else if path.starts_with("/ui") && !path.starts_with("/ui/assets/") {
        Some("no-cache")
    } else {
        None
    };
    if let Some(v) = cache {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
    }
    resp
}
