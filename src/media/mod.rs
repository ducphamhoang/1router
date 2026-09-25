//! Media generation (`/v1/images/*`). v1 serves images through Codex OAuth
//! providers only - see `docs/superpowers/plans/2026-09-25-image-generation-codex-plan.md`.

pub mod codex_images;
pub mod images;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::config::{Config, MediaConfig};

/// `server_secrets` row that persists the admin's images on/off toggle.
pub const IMAGES_ENABLED_SETTING: &str = "images_enabled";

/// Everything the media routes need beyond the chat `AppState` fields,
/// behind one `Arc` so adding media didn't touch every `AppState {` site
/// more than once.
pub struct MediaState {
    /// Separate from `AppState.http`: image calls stay silent for tens of
    /// seconds, so they need a longer idle (read) timeout than chat.
    pub http: reqwest::Client,
    pub config: MediaConfig,
    /// Off by default; flipped by `PATCH /admin/settings/images`.
    pub images_enabled: AtomicBool,
    /// Global cap on in-flight image requests (memory guard: each can
    /// buffer up to `config.max_response_bytes`).
    pub permits: tokio::sync::Semaphore,
}

impl MediaState {
    pub fn new(cfg: &Config, images_enabled: bool) -> MediaState {
        MediaState::from_parts(cfg.connect_timeout, cfg.media.clone(), images_enabled)
    }

    fn from_parts(
        connect_timeout: std::time::Duration,
        config: MediaConfig,
        images_enabled: bool,
    ) -> MediaState {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(config.idle_timeout)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_nodelay(true)
            // SEC-10, same as `core::http_client::build_client`.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build media reqwest client");
        MediaState {
            http,
            permits: tokio::sync::Semaphore::new(config.max_concurrency.max(1)),
            config,
            images_enabled: AtomicBool::new(images_enabled),
        }
    }

    pub fn images_enabled(&self) -> bool {
        self.images_enabled.load(Ordering::Relaxed)
    }
}

impl Default for MediaState {
    /// Images off, production defaults - for test `AppState` literals.
    fn default() -> Self {
        MediaState::from_parts(std::time::Duration::from_secs(10), MediaConfig::default(), false)
    }
}
