use crate::core::config::Config;

pub fn build_client(cfg: &Config) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(cfg.connect_timeout)
        // reqwest's read_timeout is an inter-read idle timeout that resets on every
        // read, not a headers-only TTFB cap — it also governs gaps between streamed
        // SSE chunks once a response is streaming. Use idle_timeout (the more
        // permissive value, meant for exactly this role) rather than ttfb_timeout,
        // so a valid slow stream isn't killed by a tighter TTFB-oriented value.
        // reqwest has no separate mechanism for a headers-only TTFB deadline, and
        // AppState holds a single shared client, so this is a deliberate v1
        // simplification: ttfb_timeout is reserved for a future distinct enforcement
        // (e.g. a second client used only up to response-headers) if that's ever
        // needed. Do NOT set an overall .timeout() — long valid streamed bodies must
        // not be killed by a deadline.
        .read_timeout(cfg.idle_timeout)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_nodelay(true)
        .build()
        .expect("failed to build reqwest client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use std::time::Duration;

    fn cfg() -> Config {
        Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "x".into(),
            seed_path: None,
            connect_timeout: Duration::from_secs(3),
            ttfb_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(7),
            max_body_bytes: 1024,
            drain_timeout: Duration::from_secs(1),
            dataset_log_dir: std::path::PathBuf::from("dataset-logs"),
        }
    }

    #[test]
    fn build_client_returns_usable_client() {
        let client = build_client(&cfg());
        // Smoke: the builder did not panic and produced a Client we can clone cheaply.
        let _c2 = client.clone();
    }
}
