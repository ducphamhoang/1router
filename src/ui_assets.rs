use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use crate::core::state::AppState;

#[derive(rust_embed::RustEmbed)]
#[folder = "frontend/dist/"]
struct Dist;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui", get(redirect_to_ui_slash))
        .route("/ui/*path", get(serve_asset))
}

async fn redirect_to_ui_slash() -> Redirect {
    Redirect::permanent("/ui/")
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    let path = path.trim_start_matches('/');
    let lookup = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Dist::get(lookup) {
        let mime = file.metadata.mimetype();
        return ([(header::CONTENT_TYPE, mime)], file.data).into_response();
    }

    match Dist::get("index.html") {
        Some(file) => ([(header::CONTENT_TYPE, "text/html")], file.data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::extract::Path;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};

    async fn test_state() -> AppState {
        let db = init_pool(":memory:").await.unwrap();
        let cfg = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "test-secret".into(),
            seed_path: None,
            connect_timeout: Duration::from_secs(1),
            ttfb_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            drain_timeout: Duration::from_secs(1),
        };
        let (log_tx, _log_rx) = tokio::sync::mpsc::channel(8);

        AppState {
            db,
            http: reqwest::Client::new(),
            config: Arc::new(cfg.clone()),
            shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret.clone())),
            secret_origin: SecretOrigin::SidecarFile,
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: vec![],
                pools: vec![],
            })),
            runtime: Arc::new(dashmap::DashMap::new()),
            log_tx,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
            login_attempts: Arc::new(dashmap::DashMap::new()),
        }
    }

    #[test]
    fn routes_registers_axum_07_named_wildcard_without_panicking() {
        let _ = super::routes();
    }

    #[tokio::test]
    async fn redirect_returns_308_to_ui_slash() {
        let resp = super::redirect_to_ui_slash().await.into_response();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/");
    }

    #[tokio::test]
    async fn serve_asset_serves_index_html_at_root() {
        let resp = super::serve_asset(Path(String::new())).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().starts_with("text/html"));

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&body).unwrap().contains("<"));
    }

    #[tokio::test]
    async fn serve_asset_falls_back_to_index_for_unknown_subpath() {
        let resp = super::serve_asset(Path("providers/deep-link".to_string())).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().starts_with("text/html"));
    }

    #[tokio::test]
    async fn ui_route_reaches_redirect_handler() {
        let app = super::routes().with_state(test_state().await);

        let resp = app
            .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/");
    }
}
